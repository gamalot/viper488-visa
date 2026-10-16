//! The operations VPP-4.3 defines for resources this runtime does not have: VXI and PXI register
//! access (viIn*, viOut*, viMove*, viMapAddress, viPeek*, viPoke*), shared memory, trigger mapping
//! and the VXI signals. They are exported so that a program linked against the full API, or the
//! IVI VISA router looking each one up, finds them; each answers VI_ERROR_NSUP_OPER for a session
//! (VI_ERROR_INV_OBJECT for anything else) and touches nothing.
//!
//! viPeek* and viPoke* return nothing by VPP-4.3's signature; with no mapping they do nothing, and
//! a peek reads 0.

use super::*;

fn nsup(vi: ViSession) -> ViStatus {
    if get(vi).is_some() { VI_ERROR_NSUP_OPER } else { VI_ERROR_INV_OBJECT }
}

macro_rules! nsup {
    ($($name:ident($($a:ident: $t:ty),*);)*) => {$(
        #[no_mangle]
        pub unsafe extern "system" fn $name(vi: ViSession, $($a: $t),*) -> ViStatus {
            $(let _ = $a;)*
            op(vi, stringify!($name), || nsup(vi))
        }
    )*};
}

type Addr = usize; // ViBusAddress and ViBusSize: 64 bits in a 64-bit VISA, 32 in a 32-bit one

nsup! {
    viIn8(space: u16, offset: Addr, v: *mut u8);
    viOut8(space: u16, offset: Addr, v: u8);
    viIn16(space: u16, offset: Addr, v: *mut u16);
    viOut16(space: u16, offset: Addr, v: u16);
    viIn32(space: u16, offset: Addr, v: *mut u32);
    viOut32(space: u16, offset: Addr, v: u32);
    viIn64(space: u16, offset: Addr, v: *mut u64);
    viOut64(space: u16, offset: Addr, v: u64);
    viIn8Ex(space: u16, offset: u64, v: *mut u8);
    viOut8Ex(space: u16, offset: u64, v: u8);
    viIn16Ex(space: u16, offset: u64, v: *mut u16);
    viOut16Ex(space: u16, offset: u64, v: u16);
    viIn32Ex(space: u16, offset: u64, v: *mut u32);
    viOut32Ex(space: u16, offset: u64, v: u32);
    viIn64Ex(space: u16, offset: u64, v: *mut u64);
    viOut64Ex(space: u16, offset: u64, v: u64);
    viMoveIn8(space: u16, offset: Addr, length: Addr, buf: *mut u8);
    viMoveOut8(space: u16, offset: Addr, length: Addr, buf: *const u8);
    viMoveIn16(space: u16, offset: Addr, length: Addr, buf: *mut u16);
    viMoveOut16(space: u16, offset: Addr, length: Addr, buf: *const u16);
    viMoveIn32(space: u16, offset: Addr, length: Addr, buf: *mut u32);
    viMoveOut32(space: u16, offset: Addr, length: Addr, buf: *const u32);
    viMoveIn64(space: u16, offset: Addr, length: Addr, buf: *mut u64);
    viMoveOut64(space: u16, offset: Addr, length: Addr, buf: *const u64);
    viMoveIn8Ex(space: u16, offset: u64, length: Addr, buf: *mut u8);
    viMoveOut8Ex(space: u16, offset: u64, length: Addr, buf: *const u8);
    viMoveIn16Ex(space: u16, offset: u64, length: Addr, buf: *mut u16);
    viMoveOut16Ex(space: u16, offset: u64, length: Addr, buf: *const u16);
    viMoveIn32Ex(space: u16, offset: u64, length: Addr, buf: *mut u32);
    viMoveOut32Ex(space: u16, offset: u64, length: Addr, buf: *const u32);
    viMoveIn64Ex(space: u16, offset: u64, length: Addr, buf: *mut u64);
    viMoveOut64Ex(space: u16, offset: u64, length: Addr, buf: *const u64);
    viMove(src_space: u16, src: Addr, src_width: u16, dst_space: u16, dst: Addr, dst_width: u16, length: Addr);
    viMoveAsync(src_space: u16, src: Addr, src_width: u16, dst_space: u16, dst: Addr, dst_width: u16, length: Addr, job: *mut u32);
    viMoveEx(src_space: u16, src: u64, src_width: u16, dst_space: u16, dst: u64, dst_width: u16, length: Addr);
    viMoveAsyncEx(src_space: u16, src: u64, src_width: u16, dst_space: u16, dst: u64, dst_width: u16, length: Addr, job: *mut u32);
    viMapAddress(space: u16, offset: Addr, size: Addr, access: u16, suggested: *mut c_void, address: *mut *mut c_void);
    viMapAddressEx(space: u16, offset: u64, size: Addr, access: u16, suggested: *mut c_void, address: *mut *mut c_void);
    viUnmapAddress();
    viMemAlloc(size: Addr, offset: *mut Addr);
    viMemFree(offset: Addr);
    viMemAllocEx(size: Addr, offset: *mut u64);
    viMemFreeEx(offset: u64);
    viVxiCommandQuery(mode: u16, cmd: u32, response: *mut u32);
    viAssertUtilSignal(line: u16);
    viAssertIntrSignal(mode: i16, status_id: u32);
    viMapTrigger(src: i16, dst: i16, mode: u16);
    viUnmapTrigger(src: i16, dst: i16);
    viPxiReserveTriggers(cnt: i16, buses: *mut i16, lines: *mut i16, failure: *mut i16);
}

macro_rules! peek {
    ($($peek:ident, $poke:ident: $t:ty;)*) => {$(
        #[no_mangle]
        pub unsafe extern "system" fn $peek(_vi: ViSession, _address: *mut c_void, v: *mut $t) {
            put(v, 0);
        }
        #[no_mangle]
        pub unsafe extern "system" fn $poke(_vi: ViSession, _address: *mut c_void, _v: $t) {}
    )*};
}

peek! {
    viPeek8, viPoke8: u8;
    viPeek16, viPoke16: u16;
    viPeek32, viPoke32: u32;
    viPeek64, viPoke64: u64;
}
