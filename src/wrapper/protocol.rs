// Copyright (C) 2016  ParadoxSpiral
//
// This file is part of mpv-rs.
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Lesser General Public
// License as published by the Free Software Foundation; either
// version 2.1 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Lesser General Public License for more details.
//
// You should have received a copy of the GNU Lesser General Public
// License along with this library; if not, write to the Free Software
// Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA  02110-1301  USA

//! This allows registering custom protocols, which then can be used via
//! `PlaylistOp::Loadfiles`.

use libc;
use parking_lot::Mutex;

use super::*;
use super::mpv_err;
use super::super::raw::*;

use std::ffi::{CStr, CString};
use std::marker::PhantomData;
use std::mem;
use std::panic;
use std::panic::RefUnwindSafe;
use std::ptr;
use std::sync::atomic::Ordering;
#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

impl Mpv {
    #[inline]
    /// Create a context with which custom protocols can be registered.
    ///
    /// Returns `None` if a context already exists
    pub fn create_protocol_context<T, U>(&self, capacity: usize) -> Option<ProtocolContext<T, U>>
        where T: RefUnwindSafe,
              U: RefUnwindSafe
    {
        if self.protocols_guard
               .compare_and_swap(false, true, Ordering::AcqRel) {
            None
        } else {
            Some(ProtocolContext::new(self.ctx, capacity, PhantomData::<&Self>))
        }
    }
}

/// Return an initialized `T`, panic on errors.
pub type StreamOpen<T, U> = fn(&mut U, &str) -> T;
/// Do any necessary cleanup.
pub type StreamClose<T> = fn(Box<T>);
/// Seek to the given offset. Return the new offset, or `MpvError::Generic` if seek failed.
pub type StreamSeek<T> = fn(&mut T, i64) -> i64;
/// Read nbytes into the given buffer.
/// Return either the number of read bytes, `0` on EOF, `-1` on error.
pub type StreamRead<T> = fn(&mut T, *mut libc::c_char, u64) -> i64;
/// Return the total size of the stream in bytes.
pub type StreamSize<T> = fn(&mut T) -> i64;

unsafe extern "C" fn open_wrapper<T, U>(user_data: *mut libc::c_void,
                                        uri: *mut libc::c_char,
                                        info: *mut MpvStreamCbInfo)
                                        -> libc::c_int
    where T: RefUnwindSafe,
          U: RefUnwindSafe
{
    let data = user_data as *mut ProtocolData<T, U>;

    (*info).cookie = user_data;
    (*info).read_fn = read_wrapper::<T, U>;
    (*info).seek_fn = seek_wrapper::<T, U>;
    (*info).size_fn = size_wrapper::<T, U>;
    (*info).close_fn = close_wrapper::<T, U>;

    let ret = panic::catch_unwind(|| {
                                      let uri = CStr::from_ptr(uri as *const _);
                                      ptr::write((*data).cookie,
                                                 ((*data).open_fn)(&mut (*data).user_data,
                                                                   &mpv_cstr_to_string!(uri)
                                                                        .unwrap()));
                                  });
    if ret.is_ok() {
        0
    } else {
        MpvError::Generic as libc::c_int
    }
}

unsafe extern "C" fn read_wrapper<T, U>(cookie: *mut libc::c_void,
                                        buf: *mut libc::c_char,
                                        nbytes: libc::uint64_t)
                                        -> libc::int64_t
    where T: RefUnwindSafe,
          U: RefUnwindSafe
{
    let data = cookie as *mut ProtocolData<T, U>;

    let ret = panic::catch_unwind(|| {
                                      debug_assert!(!(*data).cookie.is_null());
                                      ((*data).read_fn)(&mut *(*data).cookie, buf, nbytes)
                                  });
    if ret.is_ok() { ret.unwrap() } else { -1 }
}

unsafe extern "C" fn seek_wrapper<T, U>(cookie: *mut libc::c_void,
                                        offset: libc::int64_t)
                                        -> libc::int64_t
    where T: RefUnwindSafe,
          U: RefUnwindSafe
{
    let data = cookie as *mut ProtocolData<T, U>;

    if (*data).seek_fn.is_none() {
        return MpvError::Unsupported as libc::int64_t;
    }

    let ret = panic::catch_unwind(|| {
                                      debug_assert!(!(*data).cookie.is_null());
                                      (*(*data).seek_fn.as_ref().unwrap())(&mut *(*data).cookie,
                                                                           offset)
                                  });
    if ret.is_ok() {
        ret.unwrap()
    } else {
        MpvError::Generic as libc::int64_t
    }
}

unsafe extern "C" fn size_wrapper<T, U>(cookie: *mut libc::c_void) -> libc::int64_t
    where T: RefUnwindSafe,
          U: RefUnwindSafe
{
    let data = cookie as *mut ProtocolData<T, U>;

    if (*data).size_fn.is_none() {
        return MpvError::Unsupported as libc::int64_t;
    }

    let ret = panic::catch_unwind(|| {
                                      debug_assert!(!(*data).cookie.is_null());
                                      (*(*data).size_fn.as_ref().unwrap())(&mut *(*data).cookie)
                                  });
    if ret.is_ok() {
        ret.unwrap()
    } else {
        MpvError::Unsupported as libc::int64_t
    }
}

#[allow(unused_must_use)]
unsafe extern "C" fn close_wrapper<T, U>(cookie: *mut libc::c_void)
    where T: RefUnwindSafe,
          U: RefUnwindSafe
{
    let data = cookie as *mut ProtocolData<T, U>;

    panic::catch_unwind(|| {
                            debug_assert!(!(*data).cookie.is_null());
                            ((*data).close_fn)(Box::from_raw((*data).cookie))
                        });
}

struct ProtocolData<T, U> {
    cookie: *mut T,
    user_data: U,

    open_fn: StreamOpen<T, U>,
    close_fn: StreamClose<T>,
    read_fn: StreamRead<T>,
    seek_fn: Option<StreamSeek<T>>,
    size_fn: Option<StreamSize<T>>,
}

/// This context holds state relevant to custom protocols.
/// It is created by calling `Mpv::create_protocol_context`.
pub struct ProtocolContext<'parent, T: RefUnwindSafe, U: RefUnwindSafe> {
    ctx: *mut MpvHandle,
    protocols: Mutex<Vec<Protocol<T, U>>>,
    _does_not_outlive: PhantomData<&'parent Mpv>,
}

unsafe impl<'parent, T: RefUnwindSafe, U: RefUnwindSafe> Send for ProtocolContext<'parent, T, U> {}
unsafe impl<'parent, T: RefUnwindSafe, U: RefUnwindSafe> Sync for ProtocolContext<'parent, T, U> {}

impl<'parent, T: RefUnwindSafe, U: RefUnwindSafe> ProtocolContext<'parent, T, U> {
    fn new(ctx: *mut MpvHandle,
           capacity: usize,
           marker: PhantomData<&'parent Mpv>)
           -> ProtocolContext<'parent, T, U> {
        ProtocolContext {
            ctx,
            protocols: Mutex::new(Vec::with_capacity(capacity)),
            _does_not_outlive: marker,
        }
    }

    /// Register a custom `Protocol`. Once a protocol has been registered, it lives as long as
    /// `Mpv`.
    ///
    /// Returns `Error::Mpv(MpvError::InvalidParameter)` if a protocol with the same name has
    /// already been registered.
    pub fn register(&self, protocol: Protocol<T, U>) -> Result<()> {
        let mut protocols = self.protocols.lock();
        protocol.register(self.ctx)?;
        protocols.push(protocol);
        Ok(())
    }
}

/// `Protocol` holds all state used by a custom protocol.
pub struct Protocol<T: Sized + RefUnwindSafe, U: RefUnwindSafe> {
    name: String,
    data: *mut ProtocolData<T, U>,
}

impl<T: RefUnwindSafe, U: RefUnwindSafe> Protocol<T, U> {
    /// `name` is the prefix of the protocol, e.g. `myprotocol://path`.
    ///
    /// `user_data` is data that will be passed to `StreamOpen`.
    ///
    /// # Safety
    /// Do not call libmpv functions in any supplied function.
    /// All panics of the provided functions are catched and can be used as generic error returns.
    ///
    /// Panic unwinding is catched and returns an appropriate error.
    pub unsafe fn new(name: String,
                      user_data: U,
                      open_fn: StreamOpen<T, U>,
                      close_fn: StreamClose<T>,
                      read_fn: StreamRead<T>,
                      seek_fn: Option<StreamSeek<T>>,
                      size_fn: Option<StreamSize<T>>)
                      -> Protocol<T, U> {
        let data = Box::into_raw(Box::new(ProtocolData {
                                              cookie: allocate(1),
                                              user_data: user_data,

                                              open_fn: open_fn,
                                              close_fn: close_fn,
                                              read_fn: read_fn,
                                              seek_fn: seek_fn,
                                              size_fn: size_fn,
                                          }));

        Protocol {
            name: name,
            data: data,
        }
    }

    fn register(&self, ctx: *mut MpvHandle) -> Result<()> {
        let name = CString::new(&self.name[..])?;
        unsafe {
            mpv_err((),
                    mpv_stream_cb_add_ro(ctx,
                                         name.as_ptr(),
                                         self.data as *mut _,
                                         open_wrapper::<T, U> as _))
        }
    }
}

impl<T: RefUnwindSafe, U: RefUnwindSafe> Drop for Protocol<T, U> {
    fn drop(&mut self) {
        unsafe {
            Box::from_raw(self.data);
            // data.cookie will be consumed by the close callback
        };
    }
}

// Hack from https://github.com/rust-lang/rust/issues/27700#issuecomment-169014713 to not require
// nightly.
fn allocate<T>(count: usize) -> *mut T {
    let mut v = Vec::with_capacity(count);
    let ptr = v.as_mut_ptr();
    mem::forget(v);
    ptr
}
