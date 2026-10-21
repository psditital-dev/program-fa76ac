//! Unmarked fee program: collects the 0.5 % service fee every access address pays in USDC, splits it
//! between development and a UNMK buyback, and burns every UNMK it ever receives. It holds no user
//! deposits. See ../README-ORDER.md.
//!
//! Id-agnostic: every PDA and owner check uses the program id the runtime passes in, so one build runs
//! under any address (devnet rehearsals, mainnet) without a source edit.
#![cfg_attr(not(test), no_std)]

use pinocchio::cpi::{invoke_signed, invoke_signed_with_bounds, Seed, Signer};
use pinocchio::error::ProgramError;
use pinocchio::instruction::{InstructionAccount, InstructionView};
use pinocchio::sysvars::{clock::Clock, rent::Rent, Sysvar};
use pinocchio::{AccountView, Address, ProgramResult};
use pinocchio_system::instructions::{Allocate, Assign, Transfer};

#[cfg(not(feature = "no-entrypoint"))]
pinocchio::program_entrypoint!(process_instruction, 128);
pinocchio::no_allocator!();
pinocchio::nostd_panic_handler!();

const SYSTEM: Address = pinocchio_system::ID; // = 32 zero bytes
const LOADER: Address = Address::from_str_const("BPFLoaderUpgradeab1e11111111111111111111111");
const TOKEN: Address = Address::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN22: Address = Address::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const NATIVE: Address = Address::from_str_const("So11111111111111111111111111111111111111112");
const NATIVE22: Address = Address::from_str_const("9pan9bMn5HatX4EJdBwg9VgCa7Uz5HL8N1m5D3NdXejP");
/// Jupiter v6 aggregator (mainnet). Tests load a mock swap program at this address.
const JUPITER: Address = Address::from_str_const("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");

const BPS: u64 = 10_000;
const FEE_BPS: u16 = 50; // 0.5 % of each access funding (charged by the client, published here)
const FEE_MAX: u16 = 100;
const DISCOUNT_BPS: u16 = 3_000; // fee in UNMK: 30 % off -> 0.35 %
const DISCOUNT_MAX: u16 = 5_000;
const DEV_BPS: u16 = 5_000; // ordinary USDC fees: 50 % development, rest buyback-and-burn
const DEV_MAX: u16 = 5_000; // the public promise: development never gets more than half
const CAP: u64 = 1_000_000_000; // operator buyback cap per UTC day: 1,000 USDC
const CAP_MIN: u64 = 1_000_000; // 1 USDC
const CAP_MAX: u64 = 50_000_000_000; // 50,000 USDC
const DELAY: i64 = 86_400; // dev_wallet change: propose, wait 24 h, apply
const DAY: i64 = 86_400;
const TA_LEN: usize = 165; // token account without extensions (both token programs)
const MAX_ROUTE: usize = 64; // Jupiter route accounts per swap (the API default maxAccounts is 64)

const CONFIG: u8 = 1; // account discriminator
/// Config layout (byte offsets, little-endian).
mod c {
    pub const BUMP: usize = 1;
    pub const VAULT_BUMP: usize = 2; // vault authority ["vault"]
    pub const FEE_BUMP: usize = 3; // fee_vault ["fee_vault"]
    pub const BUY_BUMP: usize = 4; // buyback_vault ["buyback_vault"]
    pub const BURN_BUMP: usize = 5; // burn_vault ["burn_vault"]
    pub const PAUSED: usize = 6;
    pub const ADMIN: usize = 7;
    pub const PENDING_ADMIN: usize = 39;
    pub const OPERATOR: usize = 71;
    pub const DEV: usize = 103; // owner of the USDC account that receives the development share
    pub const PENDING_DEV: usize = 135;
    pub const DEV_ETA: usize = 167; // i64
    pub const TREASURY: usize = 175; // owner of the USDC account wind_down sweeps to
    pub const USDC: usize = 207;
    pub const UNMK: usize = 239; // zero until set_unmk_mint
    pub const UNMK_TP: usize = 271;
    pub const FEE_BPS: usize = 303; // u16
    pub const DISCOUNT_BPS: usize = 305; // u16
    pub const DEV_BPS: usize = 307; // u16
    pub const CAP: usize = 309; // u64
    pub const DAY: usize = 317; // i64 unix day of SPENT
    pub const SPENT: usize = 325; // u64 operator buyback spent that day
    pub const TOTAL_FEE: usize = 333; // USDC paid by pay_fee
    pub const FEE_COUNT: usize = 341;
    pub const TOTAL_FEE_BURN: usize = 349; // USDC spent by pay_fee_burn swaps
    pub const FEE_BURN_COUNT: usize = 357;
    pub const TOTAL_DEV: usize = 365; // USDC distributed to development
    pub const TOTAL_TO_BUYBACK: usize = 373; // USDC distributed to the buyback vault
    pub const TOTAL_BUYBACK: usize = 381; // USDC spent by buyback_burn swaps
    pub const TOTAL_BURNED: usize = 389; // UNMK base units burned
    pub const LEN: usize = 397;
}

// Event codes, first field of every `Program data:` log line.
const EV_INIT: u8 = 1;
const EV_CONFIG: u8 = 2;
const EV_PROPOSE_DEV: u8 = 3;
const EV_APPLY_DEV: u8 = 4;
const EV_PROPOSE_ADMIN: u8 = 5;
const EV_ACCEPT_ADMIN: u8 = 6;
const EV_MINT: u8 = 7;
const EV_FEE: u8 = 8;
const EV_FEE_BURN: u8 = 9;
const EV_DISTRIBUTE: u8 = 10;
const EV_BUYBACK: u8 = 11;
const EV_PAUSE: u8 = 12;
const EV_WIND_DOWN: u8 = 13;

#[repr(u32)]
enum Error {
    Unauthorized = 1,
    Paused,
    BadParams,
    BadAccount,
    MathOverflow,
    TooEarly,
    MintAlreadySet,
    MintNotSet,
    NothingToPay,
    NoPending,
    CapExceeded,
    SpentTooMuch,
    OutputTooLow,
}
fn err(e: Error) -> ProgramError {
    ProgramError::Custom(e as u32)
}
fn need(ok: bool, e: Error) -> ProgramResult {
    if ok {
        Ok(())
    } else {
        Err(err(e))
    }
}
fn rd<const N: usize>(d: &[u8], o: usize) -> Result<[u8; N], ProgramError> {
    let end = o.checked_add(N).ok_or(ProgramError::InvalidAccountData)?;
    d.get(o..end)
        .ok_or(ProgramError::InvalidAccountData)?
        .try_into()
        .map_err(|_| ProgramError::InvalidAccountData)
}
fn key(d: &[u8], o: usize) -> Result<Address, ProgramError> {
    Ok(Address::new_from_array(rd(d, o)?))
}
fn u16_at(d: &[u8], o: usize) -> Result<u16, ProgramError> {
    Ok(u16::from_le_bytes(rd(d, o)?))
}
fn u64_at(d: &[u8], o: usize) -> Result<u64, ProgramError> {
    Ok(u64::from_le_bytes(rd(d, o)?))
}
fn i64_at(d: &[u8], o: usize) -> Result<i64, ProgramError> {
    Ok(i64::from_le_bytes(rd(d, o)?))
}
fn put(d: &mut [u8], o: usize, v: &[u8]) -> ProgramResult {
    let end = o.checked_add(v.len()).ok_or(ProgramError::InvalidAccountData)?;
    d.get_mut(o..end)
        .ok_or(ProgramError::InvalidAccountData)?
        .copy_from_slice(v);
    Ok(())
}
fn add(a: u64, b: u64) -> Result<u64, ProgramError> {
    a.checked_add(b).ok_or(err(Error::MathOverflow))
}
fn sub(a: u64, b: u64) -> Result<u64, ProgramError> {
    a.checked_sub(b).ok_or(err(Error::MathOverflow))
}
/// Lifetime statistics only: saturate so a counter can never block a money flow.
fn stat(d: &mut [u8], o: usize, v: u64) -> ProgramResult {
    let x = u64_at(d, o)?.saturating_add(v);
    put(d, o, &x.to_le_bytes())
}
fn writable(a: &AccountView) -> ProgramResult {
    need(a.is_writable(), Error::BadAccount)
}
fn signer(a: &AccountView) -> ProgramResult {
    if a.is_signer() {
        Ok(())
    } else {
        Err(ProgramError::MissingRequiredSignature)
    }
}
fn now() -> Result<i64, ProgramError> {
    Ok(Clock::get()?.unix_timestamp)
}
fn rent(len: usize) -> Result<u64, ProgramError> {
    Rent::get()?.try_minimum_balance(len)
}
fn arg_u64(d: &[u8], o: usize) -> Result<u64, ProgramError> {
    u64_at(d, o).map_err(|_| ProgramError::InvalidInstructionData)
}

/// `Program data:` log line (base64 fields) the indexer reads as history.
#[cfg(target_os = "solana")]
fn emit(fields: &[&[u8]]) {
    // SAFETY: a slice of byte slices, exactly the layout sol_log_data expects.
    unsafe { pinocchio::syscalls::sol_log_data(fields.as_ptr() as *const u8, fields.len() as u64) }
}
#[cfg(not(target_os = "solana"))]
fn emit(_: &[&[u8]]) {}

fn pda(seed: &[u8], bump: u8, pid: &Address) -> Result<Address, ProgramError> {
    Address::create_program_address(&[seed, &[bump]], pid).map_err(|_| err(Error::BadAccount))
}
/// `a` is the PDA [seed] of this program with the stored bump.
fn at(a: &AccountView, seed: &[u8], bump: u8, pid: &Address) -> ProgramResult {
    need(a.address() == &pda(seed, bump, pid)?, Error::BadAccount)
}

/// Config: owned by this program, exact length, discriminator, canonical PDA (stored bump).
fn config(a: &AccountView, pid: &Address) -> Result<[u8; c::LEN], ProgramError> {
    if !a.owned_by(pid) {
        return Err(ProgramError::IllegalOwner);
    }
    if a.data_len() != c::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    let d: [u8; c::LEN] = rd(&a.try_borrow()?, 0)?;
    need(d[0] == CONFIG, Error::BadAccount)?;
    at(a, b"config", d[c::BUMP], pid)?;
    Ok(d)
}
fn save(a: &mut AccountView, d: &[u8; c::LEN]) -> ProgramResult {
    writable(a)?;
    let mut data = a.try_borrow_mut()?;
    need(data.len() == c::LEN, Error::BadAccount)?;
    data.copy_from_slice(d);
    Ok(())
}
fn admin_signed(admin: &AccountView, cd: &[u8]) -> ProgramResult {
    signer(admin)?;
    need(admin.address() == &key(cd, c::ADMIN)?, Error::Unauthorized)
}
fn live(cd: &[u8]) -> ProgramResult {
    need(cd[c::PAUSED] == 0, Error::Paused)
}

/// A wallet key the config may store (admin, operator, dev_wallet, treasury): never zero, this
/// program, or one of its own accounts (config, vault authority, the three vaults).
fn wallet(k: &Address, cd: &[u8], pid: &Address) -> ProgramResult {
    need(*k != SYSTEM && k != pid, Error::BadParams)?;
    for (seed, o) in [
        (&b"config"[..], c::BUMP),
        (b"vault", c::VAULT_BUMP),
        (b"fee_vault", c::FEE_BUMP),
        (b"buyback_vault", c::BUY_BUMP),
        (b"burn_vault", c::BURN_BUMP),
    ] {
        need(*k != pda(seed, cd[o], pid)?, Error::BadParams)?;
    }
    Ok(())
}

/// Token account of `mint` under token program `tp`, initialized (not frozen): returns (owner, amount).
fn tok(a: &AccountView, tp: &Address, mint: &Address) -> Result<(Address, u64), ProgramError> {
    need(a.owned_by(tp), Error::BadAccount)?;
    let d = a.try_borrow()?;
    // Token-2022 accounts with extensions carry the account type byte 2 at offset 165.
    let shape = d.len() == TA_LEN || (*tp == TOKEN22 && d.len() > TA_LEN && d[TA_LEN] == 2);
    need(shape && key(&d, 0)? == *mint && d[108] == 1, Error::BadAccount)?;
    Ok((key(&d, 32)?, u64_at(&d, 64)?))
}
/// One of our vaults: owner = vault authority, no delegate, no close authority. Returns the amount.
fn vault_tok(a: &AccountView, tp: &Address, mint: &Address, vault: &Address) -> Result<u64, ProgramError> {
    let (owner, amount) = tok(a, tp, mint)?;
    let d = a.try_borrow()?;
    need(owner == *vault && rd::<4>(&d, 72)? == [0; 4] && rd::<4>(&d, 129)? == [0; 4], Error::BadAccount)?;
    Ok(amount)
}
fn decimals(mint: &AccountView) -> Result<u8, ProgramError> {
    Ok(rd::<1>(&mint.try_borrow()?, 44)?[0])
}

/// Token CPI signed by the vault authority PDA.
fn vault_cpi<const N: usize>(
    tp: &Address,
    data: &[u8],
    metas: &[InstructionAccount; N],
    views: &[&AccountView; N],
    cd: &[u8],
) -> ProgramResult {
    need(*tp == TOKEN || *tp == TOKEN22, Error::BadAccount)?;
    let b = [cd[c::VAULT_BUMP]];
    let seeds = [Seed::from(b"vault"), Seed::from(&b)];
    invoke_signed(&InstructionView { program_id: tp, data, accounts: metas }, views, &[Signer::from(&seeds)])
}
/// TransferChecked out of a vault.
fn vault_transfer(
    tp: &AccountView,
    from: &AccountView,
    mint: &AccountView,
    to: &AccountView,
    vault: &AccountView,
    cd: &[u8],
    amount: u64,
) -> ProgramResult {
    if amount == 0 {
        return Ok(());
    }
    let mut data = [12u8; 10];
    data[1..9].copy_from_slice(&amount.to_le_bytes());
    data[9] = decimals(mint)?;
    let metas = [
        InstructionAccount::writable(from.address()),
        InstructionAccount::readonly(mint.address()),
        InstructionAccount::writable(to.address()),
        InstructionAccount::readonly_signer(vault.address()),
    ];
    vault_cpi(tp.address(), &data, &metas, &[from, mint, to, vault], cd)
}
/// BurnChecked of `amount` from a vault.
fn vault_burn(tp: &AccountView, from: &AccountView, mint: &AccountView, vault: &AccountView, cd: &[u8], amount: u64) -> ProgramResult {
    if amount == 0 {
        return Ok(());
    }
    let mut data = [15u8; 10];
    data[1..9].copy_from_slice(&amount.to_le_bytes());
    data[9] = decimals(mint)?;
    let metas = [
        InstructionAccount::writable(from.address()),
        InstructionAccount::writable(mint.address()),
        InstructionAccount::readonly_signer(vault.address()),
    ];
    vault_cpi(tp.address(), &data, &metas, &[from, mint, vault], cd)
}
/// CloseAccount of an empty vault, rent to `to`.
fn vault_close(tp: &AccountView, acc: &AccountView, to: &AccountView, vault: &AccountView, cd: &[u8]) -> ProgramResult {
    let metas = [
        InstructionAccount::writable(acc.address()),
        InstructionAccount::writable(to.address()),
        InstructionAccount::readonly_signer(vault.address()),
    ];
    vault_cpi(tp.address(), &[9], &metas, &[acc, to, vault], cd)
}

/// Create the token account PDA [seed] for `mint` owned by the vault authority. Allocate + assign
/// (not CreateAccount), so lamports someone sent to the address beforehand cannot block it.
#[allow(clippy::too_many_arguments)]
fn create_vault(
    payer: &AccountView,
    acc: &AccountView,
    mint: &AccountView,
    tp: &AccountView,
    seed: &[u8],
    bump: u8,
    vault: &Address,
) -> ProgramResult {
    writable(acc)?;
    if !acc.owned_by(&SYSTEM) || acc.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    let program = tp.address();
    need((*program == TOKEN || *program == TOKEN22) && mint.owned_by(program), Error::BadAccount)?;
    let shortfall = rent(TA_LEN)?.saturating_sub(acc.lamports());
    if shortfall > 0 {
        Transfer { from: payer, to: acc, lamports: shortfall }.invoke()?;
    }
    let b = [bump];
    let seeds = [Seed::from(seed), Seed::from(&b)];
    let signers = [Signer::from(&seeds)];
    Allocate { account: acc, space: TA_LEN as u64 }.invoke_signed(&signers)?;
    Assign { account: acc, owner: program }.invoke_signed(&signers)?;
    let mut data = [18u8; 33]; // InitializeAccount3 { owner }
    data[1..].copy_from_slice(vault.as_ref());
    let metas = [InstructionAccount::writable(acc.address()), InstructionAccount::readonly(mint.address())];
    invoke_signed(&InstructionView { program_id: program, data: &data, accounts: &metas }, &[acc, mint], &[])
}

/// UNMK mint policy: an initialized Token / Token-2022 mint with NO freeze authority (a frozen vault
/// could never be burned or closed). Token-2022: zero padding 82..165, account type 1, and only
/// metadata / group extensions (types 18..=23), so a 165-byte vault fits and nothing can take or block
/// it (no permanent delegate, pausable, transfer fee / hook, non-transferable, close authority).
fn mint_ok(mint: &AccountView) -> Result<bool, ProgramError> {
    let data = mint.try_borrow()?;
    let base = data.len() >= 82 && data[45] == 1 && rd::<4>(&data, 46)? == [0; 4];
    if mint.owned_by(&TOKEN) {
        return Ok(base && data.len() == 82);
    }
    if !mint.owned_by(&TOKEN22) || !base {
        return Ok(false);
    }
    if data.len() == 82 {
        return Ok(true);
    }
    if data.len() <= 166 || data[82..165].iter().any(|&b| b != 0) || data[165] != 1 {
        return Ok(false);
    }
    let mut i = 166usize;
    while i < data.len() {
        if data[i..].iter().all(|&b| b == 0) {
            return Ok(true); // trailing zero padding, of any length
        }
        let (Ok(t), Ok(l)) = (u16_at(&data, i), u16_at(&data, i + 2)) else {
            return Ok(false); // a TLV header cut by the end of the account
        };
        if t == 0 {
            return Ok(false); // a zero type followed by non-zero bytes
        }
        if !(18..=23).contains(&t) {
            return Ok(false);
        }
        i = i.checked_add(4 + l as usize).ok_or(err(Error::MathOverflow))?;
    }
    Ok(i == data.len())
}

/// Development share of `amount` (floor); the rounding dust goes to the buyback.
pub fn dev_part(amount: u64, dev_bps: u16) -> u64 {
    (amount as u128 * dev_bps as u128 / BPS as u128) as u64
}

pub fn process_instruction(pid: &Address, a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let (&tag, args) = d.split_first().ok_or(ProgramError::InvalidInstructionData)?;
    match tag {
        0 => initialize(pid, a),
        1 => update_config(pid, a, args),
        2 => propose_dev(pid, a),
        3 => apply_dev(pid, a),
        4 => propose_admin(pid, a),
        5 => accept_admin(pid, a),
        6 => set_unmk_mint(pid, a),
        7 => pay_fee(pid, a, args),
        8 => swap_ix(pid, a, args, false),
        9 => distribute(pid, a),
        10 => swap_ix(pid, a, args, true),
        11 => set_paused(pid, a, args),
        12 => wind_down(pid, a),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

/// 0. [authority(S,W), config(W), program, programdata, system, admin(S), operator, dev_wallet,
/// treasury, usdc_mint, token_program, fee_vault(W), buyback_vault(W)].
/// Only the program's upgrade authority may create the config (no init front-running); it pays the
/// rent of the config and both USDC vaults. A prefunded config/vault address is accepted.
fn initialize(pid: &Address, a: &mut [AccountView]) -> ProgramResult {
    let [authority, cfg, program, pd, sys, admin, operator, dev, treasury, usdc, tp, fee, buy, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    signer(authority)?;
    writable(authority)?;
    writable(cfg)?;
    need(sys.address() == &SYSTEM, Error::BadAccount)?;
    need(program.address() == pid && program.owned_by(&LOADER) && program.executable(), Error::Unauthorized)?;
    let pd_key = Address::find_program_address(&[pid.as_ref()], &LOADER).0;
    need(pd.address() == &pd_key && pd.owned_by(&LOADER), Error::Unauthorized)?;
    {
        let data = program.try_borrow()?;
        need(rd::<4>(&data, 0)? == 2u32.to_le_bytes() && key(&data, 4)? == pd_key, Error::Unauthorized)?;
        let data = pd.try_borrow()?;
        // ProgramData: tag 3, slot u64, Option<authority> flag@12, authority@13
        need(
            rd::<4>(&data, 0)? == 3u32.to_le_bytes() && rd::<1>(&data, 12)? == [1] && key(&data, 13)? == *authority.address(),
            Error::Unauthorized,
        )?;
    }
    signer(admin)?;
    // USDC: classic SPL Token mint, initialized.
    need(tp.address() == &TOKEN && usdc.owned_by(&TOKEN) && usdc.data_len() == 82, Error::BadAccount)?;
    need(rd::<1>(&usdc.try_borrow()?, 45)? == [1], Error::BadAccount)?;

    let mut cd = [0u8; c::LEN];
    cd[0] = CONFIG;
    for (seed, o) in [
        (&b"config"[..], c::BUMP),
        (b"vault", c::VAULT_BUMP),
        (b"fee_vault", c::FEE_BUMP),
        (b"buyback_vault", c::BUY_BUMP),
        (b"burn_vault", c::BURN_BUMP),
    ] {
        cd[o] = Address::find_program_address(&[seed], pid).1;
    }
    at(cfg, b"config", cd[c::BUMP], pid)?;
    at(fee, b"fee_vault", cd[c::FEE_BUMP], pid)?;
    at(buy, b"buyback_vault", cd[c::BUY_BUMP], pid)?;
    for k in [admin.address(), operator.address(), dev.address(), treasury.address()] {
        wallet(k, &cd, pid)?;
    }
    // Never overwrite a record; only a system-owned empty address can be initialized.
    if !cfg.owned_by(&SYSTEM) || cfg.data_len() != 0 {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    let shortfall = rent(c::LEN)?.saturating_sub(cfg.lamports());
    if shortfall > 0 {
        Transfer { from: authority, to: cfg, lamports: shortfall }.invoke()?;
    }
    let b = [cd[c::BUMP]];
    let seeds = [Seed::from(b"config"), Seed::from(&b)];
    let signers = [Signer::from(&seeds)];
    Allocate { account: cfg, space: c::LEN as u64 }.invoke_signed(&signers)?;
    Assign { account: cfg, owner: pid }.invoke_signed(&signers)?;

    let vault = pda(b"vault", cd[c::VAULT_BUMP], pid)?;
    create_vault(authority, fee, usdc, tp, b"fee_vault", cd[c::FEE_BUMP], &vault)?;
    create_vault(authority, buy, usdc, tp, b"buyback_vault", cd[c::BUY_BUMP], &vault)?;

    put(&mut cd, c::ADMIN, admin.address().as_ref())?;
    put(&mut cd, c::OPERATOR, operator.address().as_ref())?;
    put(&mut cd, c::DEV, dev.address().as_ref())?;
    put(&mut cd, c::TREASURY, treasury.address().as_ref())?;
    put(&mut cd, c::USDC, usdc.address().as_ref())?;
    put(&mut cd, c::FEE_BPS, &FEE_BPS.to_le_bytes())?;
    put(&mut cd, c::DISCOUNT_BPS, &DISCOUNT_BPS.to_le_bytes())?;
    put(&mut cd, c::DEV_BPS, &DEV_BPS.to_le_bytes())?;
    put(&mut cd, c::CAP, &CAP.to_le_bytes())?;
    save(cfg, &cd)?;
    emit(&[
        &[EV_INIT],
        admin.address().as_ref(),
        operator.address().as_ref(),
        dev.address().as_ref(),
        treasury.address().as_ref(),
        usdc.address().as_ref(),
    ]);
    Ok(())
}

/// 1. [admin(S), config(W), operator (same key if unchanged)];
/// args: fee_bps u16 (<= 100), discount_bps u16 (<= 5000), dev_split_bps u16 (<= 5000),
/// operator_daily_cap u64 (1 USDC ..= 50,000 USDC, base units). Operator change is immediate.
fn update_config(pid: &Address, a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [admin, cfg, operator, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg, pid)?;
    admin_signed(admin, &cd)?;
    let fee_bps = u16_at(d, 0).map_err(|_| ProgramError::InvalidInstructionData)?;
    let discount_bps = u16_at(d, 2).map_err(|_| ProgramError::InvalidInstructionData)?;
    let dev_bps = u16_at(d, 4).map_err(|_| ProgramError::InvalidInstructionData)?;
    let cap = arg_u64(d, 6)?;
    need(
        fee_bps <= FEE_MAX && discount_bps <= DISCOUNT_MAX && dev_bps <= DEV_MAX && (CAP_MIN..=CAP_MAX).contains(&cap),
        Error::BadParams,
    )?;
    wallet(operator.address(), &cd, pid)?;
    put(&mut cd, c::OPERATOR, operator.address().as_ref())?;
    put(&mut cd, c::FEE_BPS, &fee_bps.to_le_bytes())?;
    put(&mut cd, c::DISCOUNT_BPS, &discount_bps.to_le_bytes())?;
    put(&mut cd, c::DEV_BPS, &dev_bps.to_le_bytes())?;
    put(&mut cd, c::CAP, &cap.to_le_bytes())?;
    save(cfg, &cd)?;
    emit(&[
        &[EV_CONFIG],
        &fee_bps.to_le_bytes(),
        &discount_bps.to_le_bytes(),
        &dev_bps.to_le_bytes(),
        &cap.to_le_bytes(),
        operator.address().as_ref(),
    ]);
    Ok(())
}

/// 2. [admin(S), config(W), new_dev_wallet]. A new proposal replaces the old one and restarts the 24 h.
fn propose_dev(pid: &Address, a: &mut [AccountView]) -> ProgramResult {
    let [admin, cfg, dev, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg, pid)?;
    admin_signed(admin, &cd)?;
    wallet(dev.address(), &cd, pid)?;
    let eta = now()?.checked_add(DELAY).ok_or(err(Error::MathOverflow))?;
    put(&mut cd, c::PENDING_DEV, dev.address().as_ref())?;
    put(&mut cd, c::DEV_ETA, &eta.to_le_bytes())?;
    save(cfg, &cd)?;
    emit(&[&[EV_PROPOSE_DEV], dev.address().as_ref(), &eta.to_le_bytes()]);
    Ok(())
}

/// 3. [admin(S), config(W), new_dev_wallet = pending]. After the eta.
fn apply_dev(pid: &Address, a: &mut [AccountView]) -> ProgramResult {
    let [admin, cfg, dev, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg, pid)?;
    admin_signed(admin, &cd)?;
    let pending = key(&cd, c::PENDING_DEV)?;
    need(pending != SYSTEM, Error::NoPending)?;
    need(dev.address() == &pending, Error::Unauthorized)?;
    need(now()? >= i64_at(&cd, c::DEV_ETA)?, Error::TooEarly)?;
    put(&mut cd, c::DEV, pending.as_ref())?;
    put(&mut cd, c::PENDING_DEV, SYSTEM.as_ref())?;
    put(&mut cd, c::DEV_ETA, &0i64.to_le_bytes())?;
    save(cfg, &cd)?;
    emit(&[&[EV_APPLY_DEV], pending.as_ref()]);
    Ok(())
}

/// 4. [admin(S), config(W), new_admin]. Two-step: the new admin accepts with its own signature.
fn propose_admin(pid: &Address, a: &mut [AccountView]) -> ProgramResult {
    let [admin, cfg, new_admin, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg, pid)?;
    admin_signed(admin, &cd)?;
    wallet(new_admin.address(), &cd, pid)?;
    put(&mut cd, c::PENDING_ADMIN, new_admin.address().as_ref())?;
    save(cfg, &cd)?;
    emit(&[&[EV_PROPOSE_ADMIN], new_admin.address().as_ref()]);
    Ok(())
}

/// 5. [new_admin(S), config(W)].
fn accept_admin(pid: &Address, a: &mut [AccountView]) -> ProgramResult {
    let [new_admin, cfg, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg, pid)?;
    signer(new_admin)?;
    let pending = key(&cd, c::PENDING_ADMIN)?;
    need(pending != SYSTEM, Error::NoPending)?;
    need(new_admin.address() == &pending, Error::Unauthorized)?;
    put(&mut cd, c::ADMIN, pending.as_ref())?;
    put(&mut cd, c::PENDING_ADMIN, SYSTEM.as_ref())?;
    save(cfg, &cd)?;
    emit(&[&[EV_ACCEPT_ADMIN], pending.as_ref()]);
    Ok(())
}

/// 6. [admin(S,W), config(W), unmk_mint, token_program, burn_vault(W), system]. Once only; records the
/// mint's token program and creates the burn vault (rent paid by the admin).
fn set_unmk_mint(pid: &Address, a: &mut [AccountView]) -> ProgramResult {
    let [admin, cfg, mint, tp, burn, sys, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg, pid)?;
    admin_signed(admin, &cd)?;
    writable(admin)?;
    need(sys.address() == &SYSTEM, Error::BadAccount)?;
    need(key(&cd, c::UNMK)? == SYSTEM, Error::MintAlreadySet)?;
    let m = mint.address();
    need(*m != NATIVE && *m != NATIVE22 && *m != key(&cd, c::USDC)?, Error::BadAccount)?;
    need(mint_ok(mint)? && mint.owned_by(tp.address()), Error::BadAccount)?;
    at(burn, b"burn_vault", cd[c::BURN_BUMP], pid)?;
    let vault = pda(b"vault", cd[c::VAULT_BUMP], pid)?;
    create_vault(admin, burn, mint, tp, b"burn_vault", cd[c::BURN_BUMP], &vault)?;
    put(&mut cd, c::UNMK, m.as_ref())?;
    put(&mut cd, c::UNMK_TP, tp.address().as_ref())?;
    save(cfg, &cd)?;
    emit(&[&[EV_MINT], m.as_ref(), tp.address().as_ref()]);
    Ok(())
}

/// 7. [authority(S), config(W), source(W), fee_vault(W), usdc_mint, token_program]; args: amount u64.
/// The authority is the source's owner or its SPL delegate (the Token program enforces which and the
/// allowance). Not while paused.
fn pay_fee(pid: &Address, a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [authority, cfg, source, fee, usdc, tp, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg, pid)?;
    live(&cd)?;
    signer(authority)?;
    let amount = arg_u64(d, 0)?;
    need(amount > 0, Error::BadParams)?;
    need(tp.address() == &TOKEN && usdc.address() == &key(&cd, c::USDC)?, Error::BadAccount)?;
    at(fee, b"fee_vault", cd[c::FEE_BUMP], pid)?;
    need(source.address() != fee.address(), Error::BadAccount)?;
    let mut data = [12u8; 10];
    data[1..9].copy_from_slice(&amount.to_le_bytes());
    data[9] = decimals(usdc)?;
    let metas = [
        InstructionAccount::writable(source.address()),
        InstructionAccount::readonly(usdc.address()),
        InstructionAccount::writable(fee.address()),
        InstructionAccount::readonly_signer(authority.address()),
    ];
    invoke_signed(
        &InstructionView { program_id: &TOKEN, data: &data, accounts: &metas },
        &[&*source, &*usdc, &*fee, &*authority],
        &[],
    )?;
    stat(&mut cd, c::TOTAL_FEE, amount)?;
    stat(&mut cd, c::FEE_COUNT, 1)?;
    save(cfg, &cd)?;
    emit(&[&[EV_FEE], &amount.to_le_bytes(), &rd::<8>(&cd, c::TOTAL_FEE)?]);
    Ok(())
}

/// Swap through Jupiter into the burn vault, check the result, burn the whole burn vault.
/// Returns (USDC spent from `source`, UNMK burned).
///
/// `buyback`: the source is the buyback vault and the vault authority PDA signs the Jupiter CPI.
/// Otherwise nothing of ours signs: the payer's own signature authorizes its source.
/// Route accounts may not include the config (the vault authority may be writable: a pump.fun bonding-curve
/// buy debits the buyer's lamports), and the only USDC token
/// account owned by the vault authority a route may contain is the buyback source: the fee vault, or any
/// other USDC the PDA owns, can never be handed to Jupiter. Vault-owned accounts of other mints are allowed:
/// a plain (non-shared) Jupiter route needs the authority's own intermediates and its UNMK ATA to exist
/// (pump.fun bonding curve: "Simple AMMs are not supported with shared accounts"); what must arrive is
/// still measured in the burn vault only.
#[allow(clippy::too_many_arguments)]
fn swap_burn(
    pid: &Address,
    cfg: &AccountView,
    cd: &[u8],
    source: &AccountView,
    vault: &AccountView,
    burn: &AccountView,
    mint: &AccountView,
    tp: &AccountView,
    jup: &AccountView,
    route: &[AccountView],
    jdata: &[u8],
    max_in: u64,
    min_out: u64,
    buyback: bool,
) -> Result<(u64, u64), ProgramError> {
    need(max_in > 0 && min_out > 0, Error::BadParams)?;
    let unmk = key(cd, c::UNMK)?;
    need(unmk != SYSTEM, Error::MintNotSet)?;
    let usdc = key(cd, c::USDC)?;
    let program = tp.address();
    need(mint.address() == &unmk && *program == key(cd, c::UNMK_TP)? && mint.owned_by(program), Error::BadAccount)?;
    // address only: an account loaded through a lookup table may not carry the executable flag (seen on the fork),
    // and the runtime refuses to invoke anything that is not a program anyway
    need(jup.address() == &JUPITER, Error::BadAccount)?;
    at(vault, b"vault", cd[c::VAULT_BUMP], pid)?;
    at(burn, b"burn_vault", cd[c::BURN_BUMP], pid)?;
    writable(source)?;
    writable(burn)?;
    writable(mint)?;
    let v = vault.address();
    let source_amount = || -> Result<u64, ProgramError> {
        if buyback {
            vault_tok(source, &TOKEN, &usdc, v)
        } else {
            Ok(tok(source, &TOKEN, &usdc)?.1)
        }
    };
    let in_before = source_amount()?;
    let out_before = vault_tok(burn, program, &unmk, v)?;

    need(route.len() <= MAX_ROUTE, Error::BadAccount)?;
    for r in route {
        let k = r.address();
        need(k != cfg.address(), Error::BadAccount)?;
        if (r.owned_by(&TOKEN) || r.owned_by(&TOKEN22)) && r.data_len() >= TA_LEN {
            let d = r.try_borrow()?;
            // the only vault-owned USDC a route may touch is the buyback source
            need(key(&d, 32)? != *v || key(&d, 0)? != usdc || (buyback && k == source.address()), Error::BadAccount)?;
        }
    }
    let metas: [InstructionAccount; MAX_ROUTE] = core::array::from_fn(|i| match route.get(i) {
        Some(r) => InstructionAccount::new(r.address(), r.is_writable(), r.is_signer() || (buyback && r.address() == v)),
        None => InstructionAccount::readonly(&JUPITER), // unused tail
    });
    let ix = InstructionView { program_id: &JUPITER, data: jdata, accounts: &metas[..route.len()] };
    jupiter_cpi(&ix, route, buyback.then_some(cd[c::VAULT_BUMP]))?;

    let in_after = source_amount()?;
    let out_after = vault_tok(burn, program, &unmk, v)?;
    let spent = in_before.checked_sub(in_after).ok_or(err(Error::SpentTooMuch))?;
    need(spent > 0 && spent <= max_in, Error::SpentTooMuch)?;
    let got = out_after.checked_sub(out_before).ok_or(err(Error::OutputTooLow))?;
    need(got >= min_out, Error::OutputTooLow)?;
    vault_burn(tp, burn, mint, vault, cd, out_after)?;
    Ok((spent, out_after))
}

/// The Jupiter CPI in its own stack frame (the account array is ~3.6 KB). `vault_bump`: sign as the vault PDA.
#[inline(never)]
fn jupiter_cpi(ix: &InstructionView, route: &[AccountView], vault_bump: Option<u8>) -> ProgramResult {
    match vault_bump {
        Some(bump) => {
            let b = [bump];
            let seeds = [Seed::from(b"vault"), Seed::from(&b)];
            invoke_signed_with_bounds::<MAX_ROUTE, _>(ix, route, &[Signer::from(&seeds)])
        }
        None => invoke_signed_with_bounds::<MAX_ROUTE, _>(ix, route, &[]),
    }
}

/// 8. pay_fee_burn: [authority(S), config(W), source(W), vault, burn_vault(W), unmk_mint(W), unmk_token_program,
///    jupiter, ...route accounts]; args: usdc_amount u64, min_unmk_out u64, then the Jupiter instruction data.
///    The authority (source owner or delegate) signs the swap; at most usdc_amount leaves the source,
///    at least min_unmk_out lands in the burn vault, and the whole burn vault is burned.
/// 10. buyback_burn: [operator(S), config(W), buyback_vault(W), vault, burn_vault(W), unmk_mint(W),
///    unmk_token_program, jupiter, ...route]; same args. The vault PDA signs for the buyback vault;
///    USDC spent counts against the operator's daily cap.
fn swap_ix(pid: &Address, a: &mut [AccountView], d: &[u8], buyback: bool) -> ProgramResult {
    let [who, cfg, source, vault, burn, mint, tp, jup, route @ ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg, pid)?;
    live(&cd)?;
    signer(who)?;
    let max_in = arg_u64(d, 0)?;
    let min_out = arg_u64(d, 8)?;
    let jdata = d.get(16..).ok_or(ProgramError::InvalidInstructionData)?;
    if buyback {
        need(who.address() == &key(&cd, c::OPERATOR)?, Error::Unauthorized)?;
        at(source, b"buyback_vault", cd[c::BUY_BUMP], pid)?;
    } else {
        // the signer owns the source or is its delegate
        let (owner, _) = tok(source, &TOKEN, &key(&cd, c::USDC)?)?;
        let data = source.try_borrow()?;
        let delegate = rd::<4>(&data, 72)? == [1, 0, 0, 0] && key(&data, 76)? == *who.address();
        need(owner == *who.address() || delegate, Error::Unauthorized)?;
    }
    let (spent, burned) = swap_burn(pid, cfg, &cd, source, vault, burn, mint, tp, jup, route, jdata, max_in, min_out, buyback)?;
    stat(&mut cd, c::TOTAL_BURNED, burned)?;
    if buyback {
        let day = now()? / DAY;
        let before = if i64_at(&cd, c::DAY)? == day { u64_at(&cd, c::SPENT)? } else { 0 };
        let today = add(before, spent)?;
        need(today <= u64_at(&cd, c::CAP)?, Error::CapExceeded)?;
        put(&mut cd, c::DAY, &day.to_le_bytes())?;
        put(&mut cd, c::SPENT, &today.to_le_bytes())?;
        stat(&mut cd, c::TOTAL_BUYBACK, spent)?;
        save(cfg, &cd)?;
        emit(&[&[EV_BUYBACK], &spent.to_le_bytes(), &burned.to_le_bytes(), &today.to_le_bytes()]);
    } else {
        stat(&mut cd, c::TOTAL_FEE_BURN, spent)?;
        stat(&mut cd, c::FEE_BURN_COUNT, 1)?;
        save(cfg, &cd)?;
        emit(&[&[EV_FEE_BURN], &spent.to_le_bytes(), &burned.to_le_bytes()]);
    }
    Ok(())
}

/// 9. Permissionless, not paused. [config(W), vault, fee_vault(W), buyback_vault(W), dev_usdc(W), usdc_mint,
/// token_program]. Splits the whole fee vault: dev_split_bps to the dev_wallet's USDC account, the rest
/// (rounding dust included) to the buyback vault.
fn distribute(pid: &Address, a: &mut [AccountView]) -> ProgramResult {
    let [cfg, vault, fee, buy, dev, usdc, tp, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg, pid)?;
    live(&cd)?;
    let mint = key(&cd, c::USDC)?;
    need(tp.address() == &TOKEN && usdc.address() == &mint, Error::BadAccount)?;
    at(vault, b"vault", cd[c::VAULT_BUMP], pid)?;
    at(fee, b"fee_vault", cd[c::FEE_BUMP], pid)?;
    at(buy, b"buyback_vault", cd[c::BUY_BUMP], pid)?;
    let amount = vault_tok(fee, &TOKEN, &mint, vault.address())?;
    vault_tok(buy, &TOKEN, &mint, vault.address())?;
    need(amount > 0, Error::NothingToPay)?;
    need(tok(dev, &TOKEN, &mint)?.0 == key(&cd, c::DEV)?, Error::Unauthorized)?;
    let to_dev = dev_part(amount, u16_at(&cd, c::DEV_BPS)?);
    let to_buy = sub(amount, to_dev)?;
    vault_transfer(tp, fee, usdc, dev, vault, &cd, to_dev)?;
    vault_transfer(tp, fee, usdc, buy, vault, &cd, to_buy)?;
    stat(&mut cd, c::TOTAL_DEV, to_dev)?;
    stat(&mut cd, c::TOTAL_TO_BUYBACK, to_buy)?;
    save(cfg, &cd)?;
    emit(&[&[EV_DISTRIBUTE], &to_dev.to_le_bytes(), &to_buy.to_le_bytes()]);
    Ok(())
}

/// 11. [admin(S), config(W)]; args: paused u8 (0|1). Pause stops pay_fee, pay_fee_burn, distribute and
/// buyback_burn; admin calls and wind_down keep working.
fn set_paused(pid: &Address, a: &mut [AccountView], d: &[u8]) -> ProgramResult {
    let [admin, cfg, ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let mut cd = config(cfg, pid)?;
    admin_signed(admin, &cd)?;
    let p = *d.first().ok_or(ProgramError::InvalidInstructionData)?;
    need(p <= 1, Error::BadParams)?;
    cd[c::PAUSED] = p;
    save(cfg, &cd)?;
    emit(&[&[EV_PAUSE], &[p]]);
    Ok(())
}

/// 12. Admin, irreversible, one transaction. [admin(S,W), config(W), vault(W), fee_vault(W), buyback_vault(W),
/// treasury_usdc(W), usdc_mint, token_program, system, then once UNMK is set: burn_vault(W), unmk_mint(W),
/// unmk_token_program], then any number of [token_account(W), mint(W), token_program] triples: other
/// token accounts the vault authority owns (a plain Jupiter route's intermediates, the vault's UNMK ATA).
/// Sweeps both USDC vaults to the treasury's USDC account, burns what is left in the burn vault, closes all
/// three vaults; each extra account: USDC to the treasury, wrapped SOL just closed, any other token burned,
/// then closed. Returns any SOL on the vault authority (the float a pump.fun curve buyback needs) and closes
/// the config; all rent and SOL to the admin. Afterwards every instruction fails: the config no longer exists.
fn wind_down(pid: &Address, a: &mut [AccountView]) -> ProgramResult {
    let [admin, cfg, vault, fee, buy, treasury, usdc, tp, sys, rest @ ..] = a else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let cd = config(cfg, pid)?;
    admin_signed(admin, &cd)?;
    writable(admin)?;
    writable(cfg)?;
    let mint = key(&cd, c::USDC)?;
    need(tp.address() == &TOKEN && usdc.address() == &mint, Error::BadAccount)?;
    at(vault, b"vault", cd[c::VAULT_BUMP], pid)?;
    at(fee, b"fee_vault", cd[c::FEE_BUMP], pid)?;
    at(buy, b"buyback_vault", cd[c::BUY_BUMP], pid)?;
    need(tok(treasury, &TOKEN, &mint)?.0 == key(&cd, c::TREASURY)?, Error::Unauthorized)?;
    let from_fee = vault_tok(fee, &TOKEN, &mint, vault.address())?;
    let from_buy = vault_tok(buy, &TOKEN, &mint, vault.address())?;
    vault_transfer(tp, fee, usdc, treasury, vault, &cd, from_fee)?;
    vault_transfer(tp, buy, usdc, treasury, vault, &cd, from_buy)?;
    vault_close(tp, fee, admin, vault, &cd)?;
    vault_close(tp, buy, admin, vault, &cd)?;
    let unmk = key(&cd, c::UNMK)?;
    let mut burned = 0;
    let mut extras: &[AccountView] = rest;
    if unmk != SYSTEM {
        let [burn, umint, utp, tail @ ..] = rest else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        extras = tail;
        at(burn, b"burn_vault", cd[c::BURN_BUMP], pid)?;
        need(umint.address() == &unmk && utp.address() == &key(&cd, c::UNMK_TP)?, Error::BadAccount)?;
        burned = vault_tok(burn, utp.address(), &unmk, vault.address())?;
        vault_burn(utp, burn, umint, vault, &cd, burned)?;
        vault_close(utp, burn, admin, vault, &cd)?;
    }
    let mut from_extra = 0u64;
    for t in extras.chunks(3) {
        let [acc, m, xtp] = t else {
            return Err(ProgramError::NotEnoughAccountKeys);
        };
        let p = xtp.address();
        need((*p == TOKEN || *p == TOKEN22) && m.owned_by(p), Error::BadAccount)?;
        let (owner, amount) = tok(acc, p, m.address())?;
        need(owner == *vault.address(), Error::BadAccount)?;
        let native = rd::<4>(&acc.try_borrow()?, 109)? == [1, 0, 0, 0];
        if amount > 0 && !native {
            if *m.address() == mint {
                vault_transfer(xtp, acc, m, treasury, vault, &cd, amount)?;
                from_extra = add(from_extra, amount)?;
            } else {
                vault_burn(xtp, acc, m, vault, &cd, amount)?;
                if *m.address() == unmk {
                    burned = add(burned, amount)?;
                }
            }
        }
        vault_close(xtp, acc, admin, vault, &cd)?;
    }
    let float = vault.lamports();
    if float > 0 {
        need(sys.address() == &SYSTEM, Error::BadAccount)?;
        let b = [cd[c::VAULT_BUMP]];
        let seeds = [Seed::from(b"vault"), Seed::from(&b)];
        Transfer { from: vault, to: admin, lamports: float }.invoke_signed(&[Signer::from(&seeds)])?;
    }
    let total_burned = u64_at(&cd, c::TOTAL_BURNED)?.saturating_add(burned);
    let all = cfg.lamports();
    let got = add(admin.lamports(), all)?;
    admin.set_lamports(got);
    cfg.set_lamports(0);
    cfg.try_borrow_mut()?.fill(0);
    cfg.close()?;
    emit(&[
        &[EV_WIND_DOWN],
        &from_fee.to_le_bytes(),
        &from_buy.to_le_bytes(),
        &burned.to_le_bytes(),
        &total_burned.to_le_bytes(),
        &from_extra.to_le_bytes(),
    ]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_is_exact_and_dust_goes_to_buyback() {
        for amount in [0u64, 1, 3, 9_999, 10_001, 123_456_789, u64::MAX] {
            for bps in [0, 1, 2_500, DEV_BPS, DEV_MAX] {
                let dev = dev_part(amount, bps);
                let buy = amount - dev;
                assert_eq!(dev as u128 + buy as u128, amount as u128);
                assert!(dev as u128 * BPS as u128 <= amount as u128 * bps as u128);
                assert!((dev as u128 + 1) * BPS as u128 > amount as u128 * bps as u128);
            }
        }
        assert_eq!(dev_part(1, DEV_BPS), 0); // 1 unit: dust to the buyback
        assert_eq!(dev_part(1_000_001, DEV_BPS), 500_000);
    }

    #[test]
    fn layout_is_packed() {
        assert_eq!(c::TOTAL_BURNED + 8, c::LEN);
        assert_eq!(c::ADMIN, c::PAUSED + 1);
    }
}
