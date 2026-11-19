/*
 * Copyright (c) 2021, 2026, Oracle and/or its affiliates. All rights reserved.
 * DO NOT ALTER OR REMOVE COPYRIGHT NOTICES OR THIS FILE HEADER.
 *
 * The Universal Permissive License (UPL), Version 1.0
 *
 * Subject to the condition set forth below, permission is hereby granted to any
 * person obtaining a copy of this software, associated documentation and/or
 * data (collectively the "Software"), free of charge and under any and all
 * copyright rights in the Software, and any and all patent rights owned or
 * freely licensable by each licensor hereunder covering either (i) the
 * unmodified Software as contributed to or provided by such licensor, or (ii)
 * the Larger Works (as defined below), to deal in both
 *
 * (a) the Software, and
 *
 * (b) any piece of software and/or hardware listed in the lrgrwrks.txt file if
 * one is included with the Software each a "Larger Work" to which the Software
 * is contributed by such licensors),
 *
 * without restriction, including without limitation the rights to copy, create
 * derivative works of, display, perform, and distribute the Software and make,
 * use, sell, offer for sale, import, export, have made, and have sold the
 * Software and the Larger Work(s), and to sublicense the foregoing rights on
 * either these or other terms.
 *
 * This license is subject to the following condition:
 *
 * The above copyright notice and either this complete permission notice or at a
 * minimum a reference to the UPL must be included in all copies or substantial
 * portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

//! Thin launcher for GraalVM language standalones.
//!
//! It loads either the native image library of the language or libjvm, creates the VM through the
//! JNI invocation API and calls `AbstractLanguageLauncher.runLauncher()`.
//!
//! This is a single file without any dependency besides the Rust standard library, so that it can
//! be compiled directly with `rustc`. The few C library, OS and JNI functions it needs are
//! declared by hand below.
//!
//! The build-time configuration is passed through environment variables of the `rustc` process
//! (see `ThinLauncherProject` in `mx_sdk_vm_ng.py`):
//!
//! * required: `LAUNCHER_CLASS`, `LAUNCHER_MAIN_MODULE`, `GRAALVM_VERSION`, `LIBJVM_RELPATH`
//!   (and `LIBJLI_RELPATH` on macOS),
//! * optional: `LIBLANG_RELPATH`, `LANGUAGES_DIR`, `TOOLS_DIR`,
//! * optional lists: `LAUNCHER_MODULE_PATH`, `LAUNCHER_LIBRARY_PATH`, `LAUNCHER_OPTION_VARS`,
//!   `LAUNCHER_LANG_HOME_NAMES`, `LAUNCHER_LANG_HOME_PATHS`, `LAUNCHER_EXTRACTED_LIB_NAMES`,
//!   `LAUNCHER_EXTRACTED_LIB_PATHS`, `LAUNCHER_DEFAULT_VM_ARGS`.
//!
//! The elements of a list are separated by the ASCII unit separator (`\x1f`), which cannot occur
//! in paths, option names or VM arguments. A list variable which is not set is different from one
//! set to the empty string (a list with a single empty element), like `#ifdef` in C.
//!
//! All paths and arguments are handled as bytes: they are not necessarily valid UTF-8.

#![no_main]

use std::ffi::{c_char, c_int, c_long, c_void, CStr};
use std::io::Write;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};

/* ---------------------------------------------------------------------------------------------
 * Build-time configuration
 * ------------------------------------------------------------------------------------------- */

const LAUNCHER_CLASS: &str = env!("LAUNCHER_CLASS", "launcher class undefined");
const LAUNCHER_MAIN_MODULE: &str = env!("LAUNCHER_MAIN_MODULE", "launcher main module undefined");
const GRAALVM_VERSION: &str = env!("GRAALVM_VERSION", "GRAALVM_VERSION not defined");
const LIBJVM_RELPATH: &str = env!("LIBJVM_RELPATH", "path to jvm library undefined");
#[cfg(target_os = "macos")]
const LIBJLI_RELPATH: &str = env!("LIBJLI_RELPATH", "path to jli library undefined");
const LIBLANG_RELPATH: Option<&str> = option_env!("LIBLANG_RELPATH");
const LANGUAGES_DIR: Option<&str> = option_env!("LANGUAGES_DIR");
const TOOLS_DIR: Option<&str> = option_env!("TOOLS_DIR");

const LAUNCHER_MODULE_PATH: Option<&str> = option_env!("LAUNCHER_MODULE_PATH");
const LAUNCHER_LIBRARY_PATH: Option<&str> = option_env!("LAUNCHER_LIBRARY_PATH");
const LAUNCHER_OPTION_VARS: Option<&str> = option_env!("LAUNCHER_OPTION_VARS");
const LAUNCHER_LANG_HOME_NAMES: Option<&str> = option_env!("LAUNCHER_LANG_HOME_NAMES");
const LAUNCHER_LANG_HOME_PATHS: Option<&str> = option_env!("LAUNCHER_LANG_HOME_PATHS");
const LAUNCHER_EXTRACTED_LIB_NAMES: Option<&str> = option_env!("LAUNCHER_EXTRACTED_LIB_NAMES");
const LAUNCHER_EXTRACTED_LIB_PATHS: Option<&str> = option_env!("LAUNCHER_EXTRACTED_LIB_PATHS");
const LAUNCHER_DEFAULT_VM_ARGS: Option<&str> = option_env!("LAUNCHER_DEFAULT_VM_ARGS");

/// Separator of the elements of the list-valued configuration variables.
const LIST_SEP: char = '\x1f';

fn list(value: Option<&'static str>) -> Vec<&'static str> {
    match value {
        Some(value) => value.split(LIST_SEP).collect(),
        None => Vec::new(),
    }
}

#[cfg(windows)]
const DIR_SEP: &[u8] = b"\\";
#[cfg(not(windows))]
const DIR_SEP: &[u8] = b"/";
#[cfg(windows)]
const CP_SEP: &[u8] = b";";
#[cfg(not(windows))]
const CP_SEP: &[u8] = b":";

const VM_ARG_PREFIX: &[u8] = b"--vm.";
const VM_CP_ARG_PREFIX: &[u8] = b"--vm.cp=";
const VM_CLASSPATH_ARG_PREFIX: &[u8] = b"--vm.classpath=";
const VM_P_ARG_PREFIX: &[u8] = b"--vm.p=";
const VM_MODULE_PATH_ARG_PREFIX: &[u8] = b"--vm.-module-path=";
const VM_LIBRARY_PATH_ARG_PREFIX: &[u8] = b"--vm.Djava.library.path=";
const VM_STACK_SIZE_ARG_PREFIX: &[u8] = b"--vm.Xss";
const VM_ARG_FILE_ARG_PREFIX: &[u8] = b"--vm.@";
#[cfg(target_os = "macos")]
const VM_START_ON_FIRST_THREAD_ARG: &[u8] = b"--vm.XstartOnFirstThread";

const NMT_ARG_NAME: &[u8] = b"XX:NativeMemoryTracking";
const NMT_ENV_NAME: &[u8] = b"NMT_LEVEL_";

/* ---------------------------------------------------------------------------------------------
 * C library and OS functions
 * ------------------------------------------------------------------------------------------- */

extern "C" {
    fn getenv(name: *const c_char) -> *mut c_char;
    fn strtol(s: *const c_char, end: *mut *mut c_char, base: c_int) -> c_long;
    fn strerror(errnum: c_int) -> *mut c_char;
    fn perror(s: *const c_char);
    fn free(p: *mut c_void);
    fn exit(status: c_int) -> !;
    fn fopen(path: *const c_char, mode: *const c_char) -> *mut c_void;
    fn fread(buffer: *mut c_void, size: usize, count: usize, stream: *mut c_void) -> usize;
    fn fclose(stream: *mut c_void) -> c_int;
}

#[cfg(unix)]
mod sys {
    use std::ffi::{c_char, c_int, c_long, c_void};

    pub const RTLD_NOW: c_int = 2;
    pub const EINVAL: c_int = 22;
    #[cfg(target_os = "linux")]
    pub const SC_PAGESIZE: c_int = 30;
    #[cfg(target_os = "macos")]
    pub const SC_PAGESIZE: c_int = 29;

    /// `pthread_t` is an `unsigned long` on Linux and a pointer on macOS.
    pub type PthreadT = usize;

    /// Opaque storage for a `pthread_attr_t`, which is at most 64 bytes on the supported
    /// platforms (56 bytes on Linux x86_64, 64 bytes on Linux aarch64 and on macOS).
    #[repr(C, align(16))]
    pub struct PthreadAttr(pub [u8; 128]);

    extern "C" {
        pub fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
        pub fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        pub fn dlerror() -> *mut c_char;
        pub fn realpath(path: *const c_char, resolved: *mut c_char) -> *mut c_char;
        pub fn access(path: *const c_char, mode: c_int) -> c_int;
        pub fn setenv(name: *const c_char, value: *const c_char, overwrite: c_int) -> c_int;
        pub fn unsetenv(name: *const c_char) -> c_int;
        pub fn getpid() -> c_int;
        pub fn sysconf(name: c_int) -> c_long;
        pub fn execv(path: *const c_char, argv: *const *mut c_char) -> c_int;
        pub fn opendir(name: *const c_char) -> *mut c_void;
        pub fn closedir(dir: *mut c_void) -> c_int;
        pub fn pthread_self() -> PthreadT;
        pub fn pthread_attr_init(attr: *mut PthreadAttr) -> c_int;
        pub fn pthread_attr_destroy(attr: *mut PthreadAttr) -> c_int;
        pub fn pthread_attr_setstacksize(attr: *mut PthreadAttr, stack_size: usize) -> c_int;
        pub fn pthread_attr_setguardsize(attr: *mut PthreadAttr, guard_size: usize) -> c_int;
        pub fn pthread_create(
            thread: *mut PthreadT,
            attr: *const PthreadAttr,
            start: extern "C" fn(*mut c_void) -> *mut c_void,
            arg: *mut c_void,
        ) -> c_int;
    }

    #[cfg(target_os = "linux")]
    extern "C" {
        pub fn __errno_location() -> *mut c_int;
        pub fn pthread_getattr_np(thread: PthreadT, attr: *mut PthreadAttr) -> c_int;
        pub fn pthread_attr_getstack(attr: *const PthreadAttr, stack_addr: *mut *mut c_void, stack_size: *mut usize) -> c_int;
        pub fn pthread_join(thread: PthreadT, retval: *mut *mut c_void) -> c_int;
    }

    #[cfg(target_os = "macos")]
    extern "C" {
        pub fn __error() -> *mut c_int;
        pub fn pthread_get_stacksize_np(thread: PthreadT) -> usize;
        pub fn pthread_detach(thread: PthreadT) -> c_int;
        pub fn _NSGetExecutablePath(buf: *mut c_char, bufsize: *mut u32) -> c_int;
    }

    pub fn errno() -> c_int {
        #[cfg(target_os = "linux")]
        unsafe {
            *__errno_location()
        }
        #[cfg(target_os = "macos")]
        unsafe {
            *__error()
        }
    }

    /* Support the Cocoa event loop on the main thread */
    #[cfg(target_os = "macos")]
    pub mod cf {
        use std::ffi::c_void;

        pub const K_CF_RUN_LOOP_RUN_FINISHED: i32 = 1;

        #[link(name = "CoreFoundation", kind = "framework")]
        extern "C" {
            pub static kCFAllocatorDefault: *const c_void;
            pub static kCFRunLoopDefaultMode: *const c_void;
            pub fn CFRunLoopTimerCreate(
                allocator: *const c_void,
                fire_date: f64,
                interval: f64,
                flags: usize,
                order: isize,
                callout: extern "C" fn(*mut c_void, *mut c_void),
                context: *mut c_void,
            ) -> *mut c_void;
            pub fn CFRunLoopGetCurrent() -> *mut c_void;
            pub fn CFRunLoopAddTimer(run_loop: *mut c_void, timer: *mut c_void, mode: *const c_void);
            pub fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source_handled: u8) -> i32;
            pub fn CFRelease(cf: *const c_void);
        }
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::{c_char, c_int, c_void};

    pub const MAX_PATH: usize = 260;
    pub const GENERIC_READ: u32 = 0x8000_0000;
    pub const FILE_SHARE_READ: u32 = 1;
    pub const OPEN_EXISTING: u32 = 3;
    pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    pub const INVALID_HANDLE_VALUE: *mut c_void = -1isize as *mut c_void;
    pub const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;
    pub const STACK_SIZE_PARAM_IS_A_RESERVATION: u32 = 0x0001_0000;
    pub const INFINITE: u32 = 0xFFFF_FFFF;
    pub const P_WAIT: c_int = 0;

    /// `WIN32_FIND_DATAA`
    #[repr(C)]
    pub struct Win32FindDataA {
        pub file_attributes: u32,
        pub creation_time: [u32; 2],
        pub last_access_time: [u32; 2],
        pub last_write_time: [u32; 2],
        pub file_size_high: u32,
        pub file_size_low: u32,
        pub reserved0: u32,
        pub reserved1: u32,
        pub file_name: [c_char; MAX_PATH],
        pub alternate_file_name: [c_char; 14],
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn LoadLibraryA(name: *const c_char) -> *mut c_void;
        pub fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
        pub fn GetModuleFileNameA(module: *mut c_void, filename: *mut c_char, size: u32) -> u32;
        pub fn CreateFileA(
            name: *const c_char,
            access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: *mut c_void,
        ) -> *mut c_void;
        pub fn GetFinalPathNameByHandleA(file: *mut c_void, path: *mut c_char, size: u32, flags: u32) -> u32;
        pub fn CloseHandle(handle: *mut c_void) -> c_int;
        pub fn GetFileAttributesA(name: *const c_char) -> u32;
        pub fn FindFirstFileA(name: *const c_char, data: *mut Win32FindDataA) -> *mut c_void;
        pub fn FindNextFileA(find: *mut c_void, data: *mut Win32FindDataA) -> c_int;
        pub fn FindClose(find: *mut c_void) -> c_int;
        pub fn GetLastError() -> u32;
        pub fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        pub fn GetExitCodeThread(thread: *mut c_void, exit_code: *mut u32) -> c_int;
        pub fn GetCurrentThreadStackLimits(low_limit: *mut usize, high_limit: *mut usize);
    }

    extern "C" {
        pub fn _putenv_s(name: *const c_char, value: *const c_char) -> c_int;
        pub fn _fullpath(abs_path: *mut c_char, rel_path: *const c_char, max_length: usize) -> *mut c_char;
        pub fn _getpid() -> c_int;
        pub fn _errno() -> *mut c_int;
        pub fn _spawnv(mode: c_int, cmdname: *const c_char, argv: *const *mut c_char) -> isize;
        pub fn _beginthreadex(
            security: *mut c_void,
            stack_size: u32,
            start: extern "system" fn(*mut c_void) -> u32,
            arg: *mut c_void,
            init_flag: u32,
            thread_addr: *mut u32,
        ) -> usize;
    }

    pub fn errno() -> c_int {
        unsafe { *_errno() }
    }

    pub unsafe fn getpid() -> c_int {
        _getpid()
    }
}

/* ---------------------------------------------------------------------------------------------
 * JNI invocation API
 * ------------------------------------------------------------------------------------------- */

mod jni {
    use std::ffi::{c_char, c_void};

    pub type JInt = i32;
    pub type JLong = i64;
    pub type JBoolean = u8;
    pub type JObject = *mut c_void;
    pub type JClass = JObject;
    pub type JMethodId = *mut c_void;
    pub type JFieldId = *mut c_void;

    pub const JNI_OK: JInt = 0;
    pub const JNI_VERSION_9: JInt = 0x0009_0000;
    pub const JNI_VERSION_24: JInt = 0x0018_0000;

    #[repr(C)]
    pub struct JavaVMOption {
        pub option_string: *mut c_char,
        pub extra_info: *mut c_void,
    }

    #[repr(C)]
    pub struct JavaVMInitArgs {
        pub version: JInt,
        pub n_options: JInt,
        pub options: *mut JavaVMOption,
        pub ignore_unrecognized: JBoolean,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub union JValue {
        pub z: JBoolean,
        pub i: JInt,
        pub j: JLong,
        pub l: JObject,
    }

    /// jint JNI_CreateJavaVM(JavaVM **p_vm, void **p_env, void *vm_args);
    pub type CreateJavaVM = unsafe extern "system" fn(*mut *mut c_void, *mut *mut c_void, *mut c_void) -> JInt;
    /// jint JNI_GetDefaultJavaVMInitArgs(void *vm_args);
    pub type GetDefaultJavaVMInitArgs = unsafe extern "system" fn(*mut c_void) -> JInt;

    /// A `JNIEnv*`: a pointer to a pointer to the table of JNI functions (`struct
    /// JNINativeInterface_` in jni.h). The functions are called through their index in that
    /// table, which is fixed by the JNI specification.
    #[derive(Clone, Copy)]
    pub struct Env(pub *mut *const *const c_void);

    const FIND_CLASS: usize = 6;
    const EXCEPTION_OCCURRED: usize = 15;
    const EXCEPTION_DESCRIBE: usize = 16;
    const EXCEPTION_CLEAR: usize = 17;
    const IS_INSTANCE_OF: usize = 32;
    const GET_FIELD_ID: usize = 94;
    const GET_OBJECT_FIELD: usize = 95;
    const GET_STATIC_METHOD_ID: usize = 113;
    const CALL_STATIC_VOID_METHOD_A: usize = 143;
    const GET_STRING_UTF_CHARS: usize = 169;
    const GET_ARRAY_LENGTH: usize = 171;
    const NEW_OBJECT_ARRAY: usize = 172;
    const GET_OBJECT_ARRAY_ELEMENT: usize = 173;
    const SET_OBJECT_ARRAY_ELEMENT: usize = 174;
    const NEW_BYTE_ARRAY: usize = 176;
    const SET_BYTE_ARRAY_REGION: usize = 208;
    const EXCEPTION_CHECK: usize = 228;

    type E = *mut *const *const c_void;

    impl Env {
        unsafe fn function<F: Copy>(self, index: usize) -> F {
            debug_assert!(std::mem::size_of::<F>() == std::mem::size_of::<*const c_void>());
            std::mem::transmute_copy(&*(*self.0).add(index))
        }

        pub unsafe fn find_class(self, name: &CStr) -> JClass {
            let f: unsafe extern "system" fn(E, *const c_char) -> JClass = self.function(FIND_CLASS);
            f(self.0, name.as_ptr())
        }

        pub unsafe fn exception_occurred(self) -> JObject {
            let f: unsafe extern "system" fn(E) -> JObject = self.function(EXCEPTION_OCCURRED);
            f(self.0)
        }

        pub unsafe fn exception_describe(self) {
            let f: unsafe extern "system" fn(E) = self.function(EXCEPTION_DESCRIBE);
            f(self.0)
        }

        pub unsafe fn exception_clear(self) {
            let f: unsafe extern "system" fn(E) = self.function(EXCEPTION_CLEAR);
            f(self.0)
        }

        pub unsafe fn is_instance_of(self, object: JObject, class: JClass) -> bool {
            let f: unsafe extern "system" fn(E, JObject, JClass) -> JBoolean = self.function(IS_INSTANCE_OF);
            f(self.0, object, class) != 0
        }

        pub unsafe fn get_field_id(self, class: JClass, name: &CStr, signature: &CStr) -> JFieldId {
            let f: unsafe extern "system" fn(E, JClass, *const c_char, *const c_char) -> JFieldId = self.function(GET_FIELD_ID);
            f(self.0, class, name.as_ptr(), signature.as_ptr())
        }

        pub unsafe fn get_object_field(self, object: JObject, field: JFieldId) -> JObject {
            let f: unsafe extern "system" fn(E, JObject, JFieldId) -> JObject = self.function(GET_OBJECT_FIELD);
            f(self.0, object, field)
        }

        pub unsafe fn get_static_method_id(self, class: JClass, name: &CStr, signature: &CStr) -> JMethodId {
            let f: unsafe extern "system" fn(E, JClass, *const c_char, *const c_char) -> JMethodId = self.function(GET_STATIC_METHOD_ID);
            f(self.0, class, name.as_ptr(), signature.as_ptr())
        }

        pub unsafe fn call_static_void_method(self, class: JClass, method: JMethodId, args: &[JValue]) {
            let f: unsafe extern "system" fn(E, JClass, JMethodId, *const JValue) = self.function(CALL_STATIC_VOID_METHOD_A);
            f(self.0, class, method, args.as_ptr())
        }

        pub unsafe fn get_string_utf_chars(self, string: JObject) -> *const c_char {
            let f: unsafe extern "system" fn(E, JObject, *mut JBoolean) -> *const c_char = self.function(GET_STRING_UTF_CHARS);
            f(self.0, string, std::ptr::null_mut())
        }

        pub unsafe fn get_array_length(self, array: JObject) -> JInt {
            let f: unsafe extern "system" fn(E, JObject) -> JInt = self.function(GET_ARRAY_LENGTH);
            f(self.0, array)
        }

        pub unsafe fn new_object_array(self, length: JInt, class: JClass) -> JObject {
            let f: unsafe extern "system" fn(E, JInt, JClass, JObject) -> JObject = self.function(NEW_OBJECT_ARRAY);
            f(self.0, length, class, std::ptr::null_mut())
        }

        pub unsafe fn get_object_array_element(self, array: JObject, index: JInt) -> JObject {
            let f: unsafe extern "system" fn(E, JObject, JInt) -> JObject = self.function(GET_OBJECT_ARRAY_ELEMENT);
            f(self.0, array, index)
        }

        pub unsafe fn set_object_array_element(self, array: JObject, index: JInt, value: JObject) {
            let f: unsafe extern "system" fn(E, JObject, JInt, JObject) = self.function(SET_OBJECT_ARRAY_ELEMENT);
            f(self.0, array, index, value)
        }

        pub unsafe fn new_byte_array(self, length: JInt) -> JObject {
            let f: unsafe extern "system" fn(E, JInt) -> JObject = self.function(NEW_BYTE_ARRAY);
            f(self.0, length)
        }

        pub unsafe fn set_byte_array_region(self, array: JObject, bytes: &[u8]) {
            let f: unsafe extern "system" fn(E, JObject, JInt, JInt, *const i8) = self.function(SET_BYTE_ARRAY_REGION);
            f(self.0, array, 0, bytes.len() as JInt, bytes.as_ptr() as *const i8)
        }

        pub unsafe fn exception_check(self) -> bool {
            let f: unsafe extern "system" fn(E) -> JBoolean = self.function(EXCEPTION_CHECK);
            f(self.0) != 0
        }
    }

    use std::ffi::CStr;
}

use jni::{JValue, JavaVMInitArgs, JavaVMOption};

/* ---------------------------------------------------------------------------------------------
 * Helpers
 * ------------------------------------------------------------------------------------------- */

static DEBUG: AtomicBool = AtomicBool::new(false);
static RELAUNCH: AtomicBool = AtomicBool::new(false);
static FOUND_SWITCH_TO_JVM_FLAG: AtomicBool = AtomicBool::new(false);

fn debug() -> bool {
    DEBUG.load(Ordering::Relaxed)
}

/// Writes the concatenated parts and a newline to stdout, and flushes it.
fn out(parts: &[&[u8]]) {
    let mut stdout = std::io::stdout().lock();
    for part in parts {
        let _ = stdout.write_all(part);
    }
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

/// Writes the concatenated parts and a newline to stderr.
fn err(parts: &[&[u8]]) {
    let mut stderr = std::io::stderr().lock();
    for part in parts {
        let _ = stderr.write_all(part);
    }
    let _ = stderr.write_all(b"\n");
}

fn num<T: ToString>(n: T) -> Vec<u8> {
    n.to_string().into_bytes()
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

/// Returns a NUL-terminated copy to pass to C functions.
fn c(bytes: &[u8]) -> Vec<c_char> {
    bytes.iter().map(|&b| b as c_char).chain(std::iter::once(0)).collect()
}

unsafe fn bytes<'a>(s: *const c_char) -> &'a [u8] {
    CStr::from_ptr(s).to_bytes()
}

fn env_var(name: &[u8]) -> Option<Vec<u8>> {
    unsafe {
        let value = getenv(c(name).as_ptr());
        if value.is_null() {
            None
        } else {
            Some(bytes(value).to_vec())
        }
    }
}

fn error_string() -> Vec<u8> {
    unsafe { bytes(strerror(sys::errno())).to_vec() }
}

/// Platform-independent environment setter, use empty value to clear.
/// Returns false on failure.
fn set_env(key: &[u8], value: &[u8]) -> bool {
    if debug() {
        out(&[b"Setting env variable ", key, b"=", value]);
    }
    let (key, value_c) = (c(key), c(value));
    #[cfg(windows)]
    unsafe {
        if sys::_putenv_s(key.as_ptr(), value_c.as_ptr()) == -1 {
            err(&[b"_putenv_s failed"]);
            return false;
        }
    }
    #[cfg(unix)]
    unsafe {
        if value.is_empty() {
            /* on posix, unsetenv clears the env variable */
            if sys::unsetenv(key.as_ptr()) == -1 {
                perror(c"unsetenv failed".as_ptr());
                return false;
            }
        } else if sys::setenv(key.as_ptr(), value_c.as_ptr(), 1) == -1 {
            perror(c"setenv failed".as_ptr());
            return false;
        }
    }
    true
}

/// Checks if a file exists.
fn exists(filename: &[u8]) -> bool {
    let filename = c(filename);
    #[cfg(unix)]
    unsafe {
        sys::access(filename.as_ptr(), 0 /* F_OK */) == 0
    }
    #[cfg(windows)]
    unsafe {
        sys::GetFileAttributesA(filename.as_ptr()) != sys::INVALID_FILE_ATTRIBUTES
    }
}

/// Returns the path to the current executable.
fn exe_path() -> Vec<u8> {
    #[cfg(unix)]
    unsafe {
        #[cfg(target_os = "linux")]
        let real_path = sys::realpath(c"/proc/self/exe".as_ptr(), null_mut());
        #[cfg(target_os = "macos")]
        let real_path = {
            let mut path = [0 as c_char; 1024 /* PATH_MAX */];
            let mut path_len = path.len() as u32;
            sys::_NSGetExecutablePath(path.as_mut_ptr(), &mut path_len);
            sys::realpath(path.as_ptr(), null_mut())
        };
        if real_path.is_null() {
            err(&[b"Could not find the path of the executable: ", &error_string()]);
            exit(1);
        }
        let path = bytes(real_path).to_vec();
        free(real_path as *mut c_void);
        path
    }
    #[cfg(windows)]
    unsafe {
        let mut real_path = [0 as c_char; sys::MAX_PATH];
        sys::GetModuleFileNameA(null_mut(), real_path.as_mut_ptr(), sys::MAX_PATH as u32);
        let mut path = bytes(real_path.as_ptr()).to_vec();
        /* try to do a realpath equivalent */
        let handle = sys::CreateFileA(
            real_path.as_ptr(),
            sys::GENERIC_READ,
            sys::FILE_SHARE_READ,
            null_mut(),
            sys::OPEN_EXISTING,
            sys::FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        );
        if handle != sys::INVALID_HANDLE_VALUE {
            const SIZE: usize = sys::MAX_PATH + 4;
            let mut resolved_path = [0 as c_char; SIZE];
            let ret = sys::GetFinalPathNameByHandleA(handle, resolved_path.as_mut_ptr(), SIZE as u32, 0);
            /*
             * The path returned from GetFinalPathNameByHandleA should always
             * use "\\?\" path syntax. We strip the prefix.
             * See: https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file
             */
            if (ret as usize) < SIZE {
                let resolved = bytes(resolved_path.as_ptr());
                if resolved.starts_with(b"\\\\?\\") && resolved.len() - 4 < sys::MAX_PATH {
                    path = resolved[4..].to_vec();
                }
            }
            sys::CloseHandle(handle);
        }
        path
    }
}

/// Returns the directory of the current executable.
fn exe_directory() -> Vec<u8> {
    let path = exe_path();
    #[cfg(unix)]
    {
        /* like dirname(), for an absolute and canonical path */
        match path.iter().rposition(|&b| b == b'/') {
            Some(0) => b"/".to_vec(),
            Some(last_separator) => path[..last_separator].to_vec(),
            None => b".".to_vec(),
        }
    }
    #[cfg(windows)]
    {
        /* the drive and directory parts, including the trailing separator */
        match path.iter().rposition(|&b| b == b'\\' || b == b'/' || b == b':') {
            Some(last_separator) => path[..=last_separator].to_vec(),
            None => Vec::new(),
        }
    }
}

fn canonicalize(path: &[u8]) -> Vec<u8> {
    unsafe {
        #[cfg(unix)]
        let result = sys::realpath(c(path).as_ptr(), null_mut());
        #[cfg(windows)]
        let result = sys::_fullpath(null_mut(), c(path).as_ptr(), 0);
        if result.is_null() {
            err(&[b"Could not canonicalize ", path]);
            return path.to_vec();
        }
        let real = bytes(result).to_vec();
        free(result as *mut c_void);
        real
    }
}

/// Returns the names of the entries of a directory which do not start with a dot, or None if the
/// directory cannot be read.
fn list_directory(directory: &[u8]) -> Option<Vec<Vec<u8>>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        unsafe {
            /* same check as the names are then read: whether opendir() works */
            let dir = sys::opendir(c(directory).as_ptr());
            if dir.is_null() {
                return None;
            }
            sys::closedir(dir);
        }
        let entries = std::fs::read_dir(std::ffi::OsStr::from_bytes(directory)).ok()?;
        Some(
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().into_vec())
                .filter(|name| !name.starts_with(b"."))
                .collect(),
        )
    }
    #[cfg(windows)]
    unsafe {
        // From https://learn.microsoft.com/en-us/windows/win32/fileio/listing-the-files-in-a-directory
        // and https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-findfirstfilea
        let mut entry: sys::Win32FindDataA = std::mem::zeroed();
        let search_dir = concat(&[directory, b"\\*"]);
        let dir = sys::FindFirstFileA(c(&search_dir).as_ptr(), &mut entry);
        if dir == sys::INVALID_HANDLE_VALUE {
            return None;
        }
        let mut names = Vec::new();
        loop {
            let name = bytes(entry.file_name.as_ptr());
            if !name.starts_with(b".") {
                names.push(name.to_vec());
            }
            if sys::FindNextFileA(dir, &mut entry) == 0 {
                break;
            }
        }
        sys::FindClose(dir);
        Some(names)
    }
}

#[cfg(target_os = "macos")]
/* Load libjli - this is needed on osx for libawt, which uses JLI_* methods.
 * If the GraalVM libjli is not loaded, the osx linker will look up the symbol
 * via the JavaRuntimeSupport.framework (JRS), which will fall back to the
 * system JRE and fail if none is installed
 */
fn load_jli_lib(exe_dir: &[u8]) -> *mut c_void {
    let libjli_path = concat(&[exe_dir, DIR_SEP, LIBJLI_RELPATH.as_bytes()]);
    unsafe { sys::dlopen(c(&libjli_path).as_ptr(), sys::RTLD_NOW) }
}

/// Loads the language library (either native library or libjvm) and returns a pointer to it.
fn load_vm_lib(liblang_path: &[u8]) -> *mut c_void {
    if debug() {
        out(&[b"Loading library ", liblang_path]);
    }
    #[cfg(unix)]
    unsafe {
        let jvm_handle = sys::dlopen(c(liblang_path).as_ptr(), sys::RTLD_NOW);
        if !jvm_handle.is_null() {
            return jvm_handle;
        }
        let error_string = sys::dlerror();
        if !error_string.is_null() {
            err(&[b"Error while loading ", liblang_path, b":\n", bytes(error_string)]);
        }
        null_mut()
    }
    #[cfg(windows)]
    unsafe {
        sys::LoadLibraryA(c(liblang_path).as_ptr())
    }
}

fn get_function(library: *mut c_void, function_name: &CStr) -> *mut c_void {
    if library.is_null() {
        return null_mut();
    }
    #[cfg(unix)]
    unsafe {
        sys::dlsym(library, function_name.as_ptr())
    }
    #[cfg(windows)]
    unsafe {
        sys::GetProcAddress(library, function_name.as_ptr())
    }
}

fn vm_path(exe_dir: &[u8], jvm_mode: bool) -> Vec<u8> {
    if jvm_mode {
        concat(&[exe_dir, DIR_SEP, LIBJVM_RELPATH.as_bytes()])
    } else {
        match LIBLANG_RELPATH {
            Some(liblang_relpath) => concat(&[exe_dir, DIR_SEP, liblang_relpath.as_bytes()]),
            None => {
                err(&[b"Should not reach here: native mode with no LIBLANG defined"]);
                unsafe { exit(1) }
            }
        }
    }
}

/* ---------------------------------------------------------------------------------------------
 * VM options
 * ------------------------------------------------------------------------------------------- */

struct MainThreadArgs {
    argc: c_int,
    argv: *mut *mut c_char,
    exe_dir: Vec<u8>,
    jvm_mode: bool,
    lib_path: Vec<u8>,
    stack_size: usize,
    start_on_first_thread: bool,
    vm_args: Vec<Vec<u8>>,
    option_vars_args: Vec<Vec<u8>>,
}

/// The VM options which are accumulated while parsing the arguments.
struct VmOptions<'a> {
    vm_args: &'a mut Vec<Vec<u8>>,
    /// The class path entries, each preceded by CP_SEP.
    cp: Vec<u8>,
    /// `--module-path=` followed by the module path entries.
    module_path: Vec<u8>,
    /// The java.library.path entries, each preceded by CP_SEP.
    library_path: Vec<u8>,
    stack_size: &'a mut usize,
    start_on_first_thread: &'a mut bool,
}

impl VmOptions<'_> {
    fn parse_vm_option(&mut self, option: &[u8]) {
        if let Some(value) = option.strip_prefix(VM_CP_ARG_PREFIX) {
            self.cp.extend_from_slice(CP_SEP);
            self.cp.extend_from_slice(value);
        } else if let Some(value) = option.strip_prefix(VM_CLASSPATH_ARG_PREFIX) {
            self.cp.extend_from_slice(CP_SEP);
            self.cp.extend_from_slice(value);
        } else if let Some(value) = option.strip_prefix(VM_P_ARG_PREFIX) {
            self.module_path.extend_from_slice(CP_SEP);
            self.module_path.extend_from_slice(value);
        } else if let Some(value) = option.strip_prefix(VM_MODULE_PATH_ARG_PREFIX) {
            self.module_path.extend_from_slice(CP_SEP);
            self.module_path.extend_from_slice(value);
        } else if let Some(value) = option.strip_prefix(VM_LIBRARY_PATH_ARG_PREFIX) {
            self.library_path.extend_from_slice(CP_SEP);
            self.library_path.extend_from_slice(value);
        } else if let Some(arg_file) = option.strip_prefix(VM_ARG_FILE_ARG_PREFIX) {
            self.expand_vm_arg_file(arg_file);
        } else if self.is_start_on_first_thread(option) {
            *self.start_on_first_thread = true;
        } else if let Some(vm_option) = option.strip_prefix(VM_ARG_PREFIX) {
            if let Some(size) = option.strip_prefix(VM_STACK_SIZE_ARG_PREFIX) {
                *self.stack_size = parse_size(size);
            }
            self.vm_args.push(concat(&[b"-", vm_option]));
        } else if option == b"--jvm" {
            FOUND_SWITCH_TO_JVM_FLAG.store(true, Ordering::Relaxed);
        }
    }

    #[cfg(target_os = "macos")]
    fn is_start_on_first_thread(&self, option: &[u8]) -> bool {
        option == VM_START_ON_FIRST_THREAD_ARG
    }

    #[cfg(not(target_os = "macos"))]
    fn is_start_on_first_thread(&self, _option: &[u8]) -> bool {
        false
    }

    fn expand_vm_arg_file(&mut self, arg_file: &[u8]) {
        if debug() {
            out(&[b"Expanding VM arg file ", arg_file]);
        }
        let Some(contents) = read_file(arg_file) else {
            err(&[b"Error: could not open `", arg_file, b"': ", &error_string()]);
            unsafe { exit(1) }
        };

        let mut position = 0;
        while let Some(token) = arg_file_next_token(&contents, &mut position) {
            if let Some(value) = token.strip_prefix(b"--class-path=") {
                self.cp.extend_from_slice(CP_SEP);
                self.cp.extend_from_slice(value);
            } else if let Some(value) = token.strip_prefix(b"--module-path=") {
                self.module_path.extend_from_slice(CP_SEP);
                self.module_path.extend_from_slice(value);
            } else if let Some(value) = token.strip_prefix(b"-Djava.library.path=") {
                self.library_path.extend_from_slice(CP_SEP);
                self.library_path.extend_from_slice(value);
            } else {
                if let Some(size) = token.strip_prefix(b"-Xss") {
                    *self.stack_size = parse_size(size);
                }
                self.vm_args.push(token);
            }
        }
    }
}

/// Reads a whole file, or returns None with errno set if it cannot be opened.
fn read_file(path: &[u8]) -> Option<Vec<u8>> {
    unsafe {
        let file = fopen(c(path).as_ptr(), c"rb".as_ptr());
        if file.is_null() {
            return None;
        }
        let mut contents = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = fread(buffer.as_mut_ptr() as *mut c_void, 1, buffer.len(), file);
            if read == 0 {
                break;
            }
            contents.extend_from_slice(&buffer[..read]);
        }
        fclose(file);
        Some(contents)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ArgFileState {
    FindNext,
    InComment,
    InQuote,
    InEscape,
    SkipLeadWs,
    InToken,
}

/// Parse @arg-files as handled by libjli. See libjli/args.c.
fn arg_file_next_token(input: &[u8], position: &mut usize) -> Option<Vec<u8>> {
    use ArgFileState::*;

    let mut next = || {
        let ch = input.get(*position).copied();
        if ch.is_some() {
            *position += 1;
        }
        ch
    };

    let mut state = FindNext;
    let mut current_quote_char: Option<u8> = None;
    let mut token: Vec<u8> = Vec::new();

    'done: while let Some(mut ch) = next() {
        if state == FindNext || state == SkipLeadWs {
            // Skip white space characters
            while ch == b' ' || ch == b'\n' || ch == b'\r' || ch == b'\t' || ch == b'\x0c' {
                match next() {
                    Some(next_ch) => ch = next_ch,
                    None => break 'done,
                }
            }
            state = if state == FindNext { InToken } else { InQuote };
        } else if state == InEscape {
            // Deal with escape sequences
            if ch == b'\n' || ch == b'\r' {
                // concatenation directive
                state = SkipLeadWs;
            } else {
                // escaped character
                token.push(match ch {
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'f' => b'\x0c',
                    _ => ch,
                });
                state = InQuote;
            }
            continue;
        } else if state == InComment {
            // ignore comment to EOL
            while ch != b'\n' && ch != b'\r' {
                match next() {
                    Some(next_ch) => ch = next_ch,
                    None => break 'done,
                }
            }
            state = FindNext;
            continue;
        }

        match ch {
            b' ' | b'\t' | b'\x0c' if state == InQuote => token.push(ch),
            b' ' | b'\t' | b'\x0c' | b'\n' | b'\r' => return Some(token),
            b'#' => {
                if state == InQuote {
                    token.push(ch);
                } else {
                    state = InComment;
                }
            }
            b'\\' => {
                if state != InQuote {
                    token.push(ch);
                } else {
                    state = InEscape;
                }
            }
            b'\'' | b'"' => {
                if state == InQuote && current_quote_char != Some(ch) {
                    // not matching quote
                    token.push(ch);
                } else if state == InToken {
                    current_quote_char = Some(ch);
                    state = InQuote;
                } else {
                    state = InToken;
                }
            }
            _ => token.push(ch),
        }
    }
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Parses the VM arguments that should be passed to JNI_CreateJavaVM.
fn parse_vm_options(parsed_args: &mut MainThreadArgs) {
    let argc = parsed_args.argc;
    let argv = parsed_args.argv;
    let exe_dir = parsed_args.exe_dir.as_slice();
    let jvm_mode = parsed_args.jvm_mode;
    let option_vars_args = &mut parsed_args.option_vars_args;
    let mut options = VmOptions {
        vm_args: &mut parsed_args.vm_args,
        cp: Vec::new(),
        module_path: b"--module-path=".to_vec(),
        library_path: Vec::new(),
        stack_size: &mut parsed_args.stack_size,
        start_on_first_thread: &mut parsed_args.start_on_first_thread,
    };

    /* check if vm args have been set on relaunch already */
    let mut vm_arg_count: c_long = 0;
    if let Some(vm_arg_info) = env_var(b"GRAALVM_LANGUAGE_LAUNCHER_VMARGS") {
        RELAUNCH.store(true, Ordering::Relaxed);
        vm_arg_count = unsafe { strtol(c(&vm_arg_info).as_ptr(), null_mut(), 10) };
        // clear the env variable
        set_env(b"GRAALVM_LANGUAGE_LAUNCHER_VMARGS", b"");
    }
    let relaunch = RELAUNCH.load(Ordering::Relaxed);

    /* set system properties */
    if jvm_mode {
        /* this is only needed for jvm mode */
        options.vm_args.push(concat(&[b"-Dorg.graalvm.launcher.class=", LAUNCHER_CLASS.as_bytes()]));
    }
    options.vm_args.push(concat(&[b"-Dorg.graalvm.version=", GRAALVM_VERSION.as_bytes()]));

    let mut executablename = b"-Dorg.graalvm.launcher.executablename=".to_vec();
    if let Some(executablename_env) = env_var(b"GRAALVM_LAUNCHER_EXECUTABLE_NAME") {
        executablename.extend_from_slice(&executablename_env);
        set_env(b"GRAALVM_LAUNCHER_EXECUTABLE_NAME", b"");
    } else if argc > 0 {
        executablename.extend_from_slice(unsafe { bytes(*argv) });
    }
    if debug() {
        out(&[b"org.graalvm.launcher.executablename set to '", &executablename, b"'"]);
    }
    options.vm_args.push(executablename);

    /* construct module path - only needed for jvm mode */
    if let (true, Some(_)) = (jvm_mode, LAUNCHER_MODULE_PATH) {
        /* add the launcher module path */
        for (i, entry) in list(LAUNCHER_MODULE_PATH).iter().enumerate() {
            if i > 0 {
                options.module_path.extend_from_slice(CP_SEP);
            }
            let entry = canonicalize(&concat(&[exe_dir, DIR_SEP, entry.as_bytes()]));
            options.module_path.extend_from_slice(&entry);
        }
    }

    if let (true, Some(languages_dir), Some(tools_dir)) = (jvm_mode, LANGUAGES_DIR, TOOLS_DIR) {
        /* Add languages and tools to module path */
        for relative_dir in [languages_dir, tools_dir] {
            let absolute_dir = concat(&[exe_dir, DIR_SEP, relative_dir.as_bytes()]);
            if let Some(names) = list_directory(&absolute_dir) {
                let canonical_dir = canonicalize(&absolute_dir);
                for name in names {
                    options.module_path.extend_from_slice(&concat(&[CP_SEP, &canonical_dir, DIR_SEP, &name]));
                }
            }
        }
    }

    /* construct java.library.path - only needed for jvm mode */
    if jvm_mode {
        /* add the library path */
        for entry in list(LAUNCHER_LIBRARY_PATH) {
            options.library_path.extend_from_slice(&concat(&[CP_SEP, exe_dir, DIR_SEP, entry.as_bytes()]));
        }
    }

    if let (Some(_), Some(_)) = (LAUNCHER_LANG_HOME_NAMES, LAUNCHER_LANG_HOME_PATHS) {
        let paths = list(LAUNCHER_LANG_HOME_PATHS);
        for (name, path) in list(LAUNCHER_LANG_HOME_NAMES).iter().zip(paths) {
            let home = canonicalize(&concat(&[exe_dir, DIR_SEP, path.as_bytes()]));
            options.vm_args.push(concat(&[b"-Dorg.graalvm.language.", name.as_bytes(), b".home=", &home]));
        }
    }

    if let (true, Some(_), Some(_)) = (jvm_mode, LAUNCHER_EXTRACTED_LIB_NAMES, LAUNCHER_EXTRACTED_LIB_PATHS) {
        let paths = list(LAUNCHER_EXTRACTED_LIB_PATHS);
        for (name, path) in list(LAUNCHER_EXTRACTED_LIB_NAMES).iter().zip(paths) {
            let lib = canonicalize(&concat(&[exe_dir, DIR_SEP, path.as_bytes()]));
            options.vm_args.push(concat(&[b"-D", name.as_bytes(), b"=", &lib]));
        }
    }

    /* Handle launcher default vm arguments. We apply these first, so they can
       be overridden by explicit arguments on the commandline.
       These should be added even if relaunch is true because they are not passed to preprocessArguments(). */
    for default_vm_arg in list(LAUNCHER_DEFAULT_VM_ARGS) {
        if default_vm_arg.as_bytes().starts_with(VM_ARG_PREFIX) {
            options.parse_vm_option(default_vm_arg.as_bytes());
        }
    }

    if !relaunch {
        /* handle CLI arguments */
        for i in 1..argc {
            options.parse_vm_option(unsafe { bytes(*argv.add(i as usize)) });
        }

        /* handle optional vm args from LanguageLibraryConfig.option_vars */
        for launcher_option_var in list(LAUNCHER_OPTION_VARS) {
            let Some(option_var) = env_var(launcher_option_var.as_bytes()) else {
                continue;
            };
            if debug() {
                out(&[b"Launcher option_var found: ", launcher_option_var.as_bytes(), b"=", &option_var]);
            }
            // we split on spaces
            for option in option_var.split(|&b| b == b' ') {
                option_vars_args.push(option.to_vec());
                options.parse_vm_option(option);
            }
        }
    } else {
        /* Handle relaunch arguments. In that case GRAALVM_LANGUAGE_LAUNCHER_VMARGS_* contain all --vm.* arguments
           returned by preprocessArguments(), so we should not look at CLI args and option_vars as that would cause
           to add extra duplicate --vm.* arguments. */
        if debug() {
            out(&[b"Relaunch environment variable detected"]);
        }
        for i in 0..vm_arg_count {
            let env_key = concat(&[b"GRAALVM_LANGUAGE_LAUNCHER_VMARGS_", &num(i)]);
            let Some(cur) = env_var(&env_key) else {
                err(&[b"VM arguments specified: ", &num(vm_arg_count), b" but argument ", &num(i), b"missing"]);
                break;
            };
            options.parse_vm_option(&cur);
            /* clean up env variable */
            set_env(&env_key, b"");
        }
    }

    /* set classpath and module path arguments - only needed for jvm mode */
    if jvm_mode {
        if !options.cp.is_empty() {
            options.vm_args.push(concat(&[b"-Djava.class.path=", &options.cp[CP_SEP.len()..]]));
        }
        if !options.library_path.is_empty() {
            options.vm_args.push(concat(&[b"-Djava.library.path=", &options.library_path[CP_SEP.len()..]]));
        }
        if LAUNCHER_MODULE_PATH.is_some() {
            let module_path = std::mem::take(&mut options.module_path);
            options.vm_args.push(module_path);
            options.vm_args.push(concat(&[b"-Djdk.module.main=", LAUNCHER_MAIN_MODULE.as_bytes()]));
            options.vm_args.push(b"-Dgraalvm.locatorDisabled=true".to_vec());
        }

        /* Allow Truffle NFI Panama to use Linker#{downcallHandle,upcallStub} without warnings. */
        options.vm_args.push(b"--enable-native-access=org.graalvm.truffle".to_vec());
    }
}

/// Parses the size part of -Xss, -Xmx, etc. options.
/// Returns the parsed size in bytes or 0 if invalid.
/// Inspired by: https://github.com/openjdk/jdk/blob/8c1b915c7ef2b3a6e65705b91f4eb464caaec4e7/src/java.base/share/native/libjli/java.c#L920-L954
fn parse_size(str: &[u8]) -> usize {
    let mut n: usize = 0;
    let mut pos = 0;
    while pos < str.len() && str[pos].is_ascii_digit() {
        let digit = (str[pos] - b'0') as usize;
        n = n.wrapping_mul(10).wrapping_add(digit);
        pos += 1;
    }
    if pos == 0 || str.len() - pos > 1 {
        // invalid
        return 0;
    }
    if pos == str.len() {
        return n;
    }
    let multiplier: usize = match str[pos] {
        b'T' | b't' => 1024 * 1024 * 1024 * 1024,
        b'G' | b'g' => 1024 * 1024 * 1024,
        b'M' | b'm' => 1024 * 1024,
        b'K' | b'k' => 1024,
        // invalid
        _ => return 0,
    };
    n.wrapping_mul(multiplier)
}

/* ---------------------------------------------------------------------------------------------
 * Threads
 * ------------------------------------------------------------------------------------------- */

#[cfg(unix)]
fn round_to_pagesize(stack_size: usize) -> usize {
    let page_size = unsafe { sys::sysconf(sys::SC_PAGESIZE) } as usize;
    let remainder = stack_size % page_size;
    if remainder == 0 {
        stack_size
    } else {
        // Round up to the next full page
        stack_size - remainder + (if stack_size <= usize::MAX - page_size { page_size } else { 0 })
    }
}

/// Sets the requested thread stack size (0 = default).
#[cfg(unix)]
unsafe fn setstacksize(attr: *mut sys::PthreadAttr, stack_size: usize) -> c_int {
    // See: https://github.com/openjdk/jdk/blob/8c1b915c7ef2b3a6e65705b91f4eb464caaec4e7/src/java.base/unix/native/libjli/java_md.c#L675-L684
    if stack_size > 0 {
        let mut result = sys::pthread_attr_setstacksize(attr, stack_size);
        if result == sys::EINVAL {
            // System may require stack size to be multiple of page size
            // Retry with adjusted value
            let adjusted_stack_size = round_to_pagesize(stack_size);
            if adjusted_stack_size != stack_size {
                result = sys::pthread_attr_setstacksize(attr, adjusted_stack_size);
            }
        }
        result
    } else {
        /* Inherit stacksize of the main thread. Otherwise pthread_create()
         * defaults to 512K on darwin, while the main thread has usually ~8M.
         */
        #[cfg(target_os = "macos")]
        {
            sys::pthread_attr_setstacksize(attr, current_thread_stack_size())
        }
        #[cfg(not(target_os = "macos"))]
        {
            0
        }
    }
}

/// Returns the stack size of the current thread, or 0 if not supported.
fn current_thread_stack_size() -> usize {
    #[cfg(target_os = "macos")]
    unsafe {
        sys::pthread_get_stacksize_np(sys::pthread_self())
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let mut current_thread_stack_size = 0;
        let mut attr = sys::PthreadAttr([0; 128]);
        let mut stack_addr = null_mut();
        if sys::pthread_getattr_np(sys::pthread_self(), &mut attr) == 0 {
            sys::pthread_attr_getstack(&attr, &mut stack_addr, &mut current_thread_stack_size);
            sys::pthread_attr_destroy(&mut attr);
        }
        current_thread_stack_size
    }
    #[cfg(windows)]
    unsafe {
        /* from the lowest address of the reserved stack to the stack base */
        let (mut low, mut high) = (0, 0);
        sys::GetCurrentThreadStackLimits(&mut low, &mut high);
        high - low
    }
}

/*
 * See: https://github.com/openjdk/jdk/blob/8c1b915c7ef2b3a6e65705b91f4eb464caaec4e7/src/java.base/macosx/native/libjli/java_md_macosx.m#L277-L290
 */
#[cfg(target_os = "macos")]
extern "C" fn dummy_timer(_timer: *mut c_void, _info: *mut c_void) {}

#[cfg(target_os = "macos")]
fn park_event_loop() {
    use sys::cf::*;
    unsafe {
        // RunLoop needs at least one source, and 1e20 is pretty far into the future
        let t = CFRunLoopTimerCreate(kCFAllocatorDefault, 1.0e20, 0.0, 0, 0, dummy_timer, null_mut());
        CFRunLoopAddTimer(CFRunLoopGetCurrent(), t, kCFRunLoopDefaultMode);
        CFRelease(t);

        // Park this thread in the main run loop.
        loop {
            let result = CFRunLoopRunInMode(kCFRunLoopDefaultMode, 1.0e20, 0);
            if result == K_CF_RUN_LOOP_RUN_FINISHED {
                break;
            }
        }
    }
}

#[cfg(unix)]
extern "C" fn jvm_main_thread_start(arg: *mut c_void) -> *mut c_void {
    let mut args = unsafe { Box::from_raw(arg as *mut MainThreadArgs) };
    let ret = jvm_main_thread(&mut args);
    #[cfg(target_os = "macos")]
    unsafe {
        exit(ret)
    }
    #[cfg(not(target_os = "macos"))]
    {
        ret as isize as *mut c_void
    }
}

#[cfg(windows)]
extern "system" fn jvm_main_thread_start(arg: *mut c_void) -> u32 {
    let mut args = unsafe { Box::from_raw(arg as *mut MainThreadArgs) };
    jvm_main_thread(&mut args) as u32
}

/// Runs the JVM in a new "main" thread with the given stack size and returns its result.
fn run_in_new_thread(parsed_args: MainThreadArgs) -> c_int {
    let stack_size = parsed_args.stack_size;
    let thread_args = Box::into_raw(Box::new(parsed_args));

    #[cfg(unix)]
    unsafe {
        let mut attr = sys::PthreadAttr([0; 128]);
        if sys::pthread_attr_init(&mut attr) != 0 {
            err(&[b"Could not initialize pthread attribute structure: ", &error_string()]);
            return -1;
        }
        if setstacksize(&mut attr, stack_size) != 0 {
            err(&[b"Could not set stack size in pthread attribute structure to ", &num(stack_size), b" bytes."]);
            return -1;
        }

        // See: https://github.com/openjdk/jdk/blob/8c1b915c7ef2b3a6e65705b91f4eb464caaec4e7/src/java.base/unix/native/libjli/java_md.c#L685
        sys::pthread_attr_setguardsize(&mut attr, 0); // no pthread guard page on java threads

        let mut main_thr: sys::PthreadT = 0;
        // on success, the ownership of thread_args is transferred to the new thread
        if sys::pthread_create(&mut main_thr, &attr, jvm_main_thread_start, thread_args as *mut c_void) != 0 {
            err(&[b"Could not create main thread: ", &error_string()]);
            sys::pthread_attr_destroy(&mut attr);
            drop(Box::from_raw(thread_args));
            return -1;
        }
        sys::pthread_attr_destroy(&mut attr);

        #[cfg(target_os = "macos")]
        {
            if sys::pthread_detach(main_thr) != 0 {
                err(&[b"pthread_detach() failed: ", &error_string()]);
                return -1;
            }

            park_event_loop();
            0
        }
        #[cfg(not(target_os = "macos"))]
        {
            let mut retval = null_mut();
            if sys::pthread_join(main_thr, &mut retval) != 0 {
                err(&[b"pthread_join() failed: ", &error_string()]);
                return -1;
            }
            retval as isize as c_int
        }
    }
    #[cfg(windows)]
    unsafe {
        let thread = sys::_beginthreadex(
            null_mut(),
            stack_size as u32,
            jvm_main_thread_start,
            thread_args as *mut c_void,
            sys::STACK_SIZE_PARAM_IS_A_RESERVATION,
            null_mut(),
        );
        if thread == 0 {
            err(&[b"_beginthreadex() failed: ", &num(sys::GetLastError())]);
            drop(Box::from_raw(thread_args));
            return -1;
        }
        // the ownership of thread_args is transferred to the new thread

        // join thread
        let mut exit_code = 0;
        sys::WaitForSingleObject(thread as *mut c_void, sys::INFINITE);
        sys::GetExitCodeThread(thread as *mut c_void, &mut exit_code);
        sys::CloseHandle(thread as *mut c_void);
        exit_code as c_int
    }
}

/* ---------------------------------------------------------------------------------------------
 * Entry points
 * ------------------------------------------------------------------------------------------- */

#[no_mangle]
pub extern "C" fn main(argc: c_int, argv: *mut *mut c_char) -> c_int {
    DEBUG.store(env_var(b"VERBOSE_GRAALVM_LAUNCHERS").is_some(), Ordering::Relaxed);
    let exe_dir = exe_directory();
    let jvm_mode_env = env_var(b"GRAALVM_LAUNCHER_FORCE_JVM");
    let mut jvm_mode = jvm_mode_env.as_deref() == Some(b"true");
    if LIBLANG_RELPATH.is_none() {
        if jvm_mode_env.is_some() && !jvm_mode {
            err(&[b"Cannot run in native mode from jvm-only launcher"]);
            return -1;
        }
        jvm_mode = true;
    }

    let mut lib_path = vm_path(&exe_dir, jvm_mode);

    /* check if the VM library exists */
    if !jvm_mode && !exists(&lib_path) {
        /* switch to JVM mode */
        lib_path = vm_path(&exe_dir, true);
        jvm_mode = true;
    }

    /* parse VM args */
    let mut parsed_args = MainThreadArgs {
        argc,
        argv,
        exe_dir,
        jvm_mode,
        lib_path,
        stack_size: 0,
        start_on_first_thread: false,
        vm_args: Vec::new(),
        option_vars_args: Vec::new(),
    };
    parse_vm_options(&mut parsed_args);
    let stack_size = parsed_args.stack_size;

    /*
     * If -Xss is greater than the os-allocated stack size of the main thread,
     * create a new "main" thread for the JVM with increased stack size.
     *
     * Unlike the `java` launcher, which always creates a new thread for the JVM by default [1],
     * we create a new thread only if needed; otherwise, we attach the JVM to the main thread.
     * On macOS, it is always needed because the actual main thread must run the UI event loop [2].
     *
     * [1] https://github.com/openjdk/jdk/blob/8c1b915c7ef2b3a6e65705b91f4eb464caaec4e7/src/java.base/unix/native/libjli/java_md.c#L114
     * [2] https://github.com/openjdk/jdk/blob/8c1b915c7ef2b3a6e65705b91f4eb464caaec4e7/src/java.base/macosx/native/libjli/java_md_macosx.m#L292-L325
     */
    let main_thread_stack_size = current_thread_stack_size();
    #[cfg(not(target_os = "macos"))]
    let use_new_thread = stack_size > main_thread_stack_size;
    /* On macOS, default to creating a dedicated "main" thread for the JVM.
     * The actual main thread must run the UI event loop (needed for AWT).
     * It can be overridden with -XstartOnFirstThread, this is needed to
     * use other UI frameworks that *do* need to run on the main thread.
     */
    #[cfg(target_os = "macos")]
    let use_new_thread = !parsed_args.start_on_first_thread;

    #[cfg(target_os = "macos")]
    if jvm_mode && load_jli_lib(&parsed_args.exe_dir).is_null() {
        err(&[b"Loading libjli failed."]);
        return -1;
    }

    if use_new_thread {
        if debug() {
            out(&[
                b"Creating a new thread for the JVM with stack_size=",
                &num(stack_size),
                b" main_thread_stack_size=",
                &num(main_thread_stack_size),
            ]);
        }
        return run_in_new_thread(parsed_args);
    }
    jvm_main_thread(&mut parsed_args)
}

fn jvm_main_thread(parsed_args: &mut MainThreadArgs) -> c_int {
    let argc_native = parsed_args.argc;
    let argv_native = parsed_args.argv;
    let jvm_mode = parsed_args.jvm_mode;
    let relaunch = RELAUNCH.load(Ordering::Relaxed);

    /* load VM library - after parsing arguments s.t. NMT
     * tracking environment variables are already set */
    let library = load_vm_lib(&parsed_args.lib_path);
    if library.is_null() {
        err(&[b"Could not load VM library from ", &parsed_args.lib_path, b"."]);
        return -1;
    }

    if jvm_mode {
        let get_default_java_vm_init_args = get_function(library, c"JNI_GetDefaultJavaVMInitArgs");
        if get_default_java_vm_init_args.is_null() {
            err(&[b"Could not find JNI_GetDefaultJavaVMInitArgs."]);
            return -1;
        }
        let get_default_java_vm_init_args: jni::GetDefaultJavaVMInitArgs = unsafe { std::mem::transmute(get_default_java_vm_init_args) };

        let mut default_args = JavaVMInitArgs {
            version: jni::JNI_VERSION_24,
            n_options: 0,
            options: null_mut(),
            ignore_unrecognized: 0,
        };
        let jdk24_or_higher = unsafe { get_default_java_vm_init_args(&mut default_args as *mut JavaVMInitArgs as *mut c_void) } == jni::JNI_OK;

        if jdk24_or_higher {
            // GR-59703: Migrate sun.misc.* usages.
            parsed_args.vm_args.push(b"--sun-misc-unsafe-memory-access=allow".to_vec());
        }
    }

    // Convert vm_args to JavaVMInitArgs
    let mut option_strings: Vec<Vec<c_char>> = Vec::new();
    let mut options: Vec<JavaVMOption> = Vec::new();

    let mut svm_error: *const c_char = null();
    if !jvm_mode {
        option_strings.push(c(b"_createvm_errorstr"));
        options.push(JavaVMOption {
            option_string: null_mut(),
            extra_info: &mut svm_error as *mut *const c_char as *mut c_void,
        });
    }

    for arg in &parsed_args.vm_args {
        if debug() {
            out(&[b"Setting VM argument ", arg]);
        }
        /* env variable for native memory tracking (NMT), obsolete with JDK 18 */
        if let Some(nmt_pos) = arg.windows(NMT_ARG_NAME.len()).position(|window| window == NMT_ARG_NAME) {
            let val = arg.get(nmt_pos + NMT_ARG_NAME.len() + 1..).unwrap_or(b"");
            let pid = unsafe { sys::getpid() };
            set_env(&concat(&[NMT_ENV_NAME, &num(pid)]), val);
        }
        option_strings.push(c(arg));
        options.push(JavaVMOption { option_string: null_mut(), extra_info: null_mut() });
    }
    for (option, option_string) in options.iter_mut().zip(option_strings.iter_mut()) {
        option.option_string = option_string.as_mut_ptr();
    }

    let create_vm = get_function(library, c"JNI_CreateJavaVM");
    if create_vm.is_null() {
        err(&[b"Could not find JNI_CreateJavaVM."]);
        return -1;
    }
    let create_vm: jni::CreateJavaVM = unsafe { std::mem::transmute(create_vm) };

    let mut vm_init_args = JavaVMInitArgs {
        version: jni::JNI_VERSION_9,
        n_options: options.len() as jni::JInt,
        options: options.as_mut_ptr(),
        /* In general we want to validate VM arguments.
         * But we must disable it for the case there is a native library and we saw a --jvm argument,
         * as the VM arguments are then JVM VM arguments and not SVM VM arguments.
         * In that case we validate them after the execve() when running in --jvm mode. */
        ignore_unrecognized: (FOUND_SWITCH_TO_JVM_FLAG.load(Ordering::Relaxed) && !jvm_mode) as jni::JBoolean,
    };

    let mut vm: *mut c_void = null_mut();
    let mut env: *mut c_void = null_mut();
    if unsafe { create_vm(&mut vm, &mut env, &mut vm_init_args as *mut JavaVMInitArgs as *mut c_void) } != jni::JNI_OK {
        if !svm_error.is_null() {
            unsafe {
                err(&[bytes(svm_error)]);
                free(svm_error as *mut c_void);
            }
        }
        err(&[b"JNI_CreateJavaVM() failed."]);
        return -1;
    }

    /* free the allocated vm arguments */
    drop(options);
    drop(option_strings);

    let env = jni::Env(env as *mut *const *const c_void);
    unsafe { run_launcher(env, argc_native, argv_native, &parsed_args.option_vars_args, relaunch) }
}

/// Reports a failed JNI lookup.
unsafe fn not_found(env: jni::Env, message: &[u8]) -> c_int {
    err(&[message]);
    if env.exception_check() {
        env.exception_describe();
    }
    -1
}

/// Reports a pending exception after a JNI call, if there is one.
unsafe fn exception_in(env: jni::Env, function: &[u8]) -> bool {
    if env.exception_check() {
        err(&[b"Error in ", function, b":"]);
        env.exception_describe();
        true
    } else {
        false
    }
}

/// Creates a `byte[][]` from the given arguments.
unsafe fn new_byte_array_array<'a>(
    env: jni::Env,
    byte_array_class: jni::JClass,
    length: usize,
    elements: impl Iterator<Item = &'a [u8]>,
) -> Option<jni::JObject> {
    let array = env.new_object_array(length as jni::JInt, byte_array_class);
    for (i, element) in elements.enumerate() {
        let arg = env.new_byte_array(element.len() as jni::JInt);
        env.set_byte_array_region(arg, element);
        if exception_in(env, b"SetByteArrayRegion") {
            return None;
        }
        env.set_object_array_element(array, i as jni::JInt, arg);
        if exception_in(env, b"SetObjectArrayElement") {
            return None;
        }
    }
    Some(array)
}

unsafe fn run_launcher(
    env: jni::Env,
    argc_native: c_int,
    argv_native: *mut *mut c_char,
    option_vars_args: &[Vec<u8>],
    relaunch: bool,
) -> c_int {
    let byte_array_class = env.find_class(c"[B");
    if byte_array_class.is_null() {
        return not_found(env, b"Byte array class not found.");
    }
    let relaunch_exception_class = env.find_class(c"org/graalvm/launcher/AbstractLanguageLauncher$RelaunchException");
    if relaunch_exception_class.is_null() {
        return not_found(env, b"RelaunchException class not found.");
    }
    let launcher_class = env.find_class(c"org/graalvm/launcher/AbstractLanguageLauncher");
    if launcher_class.is_null() {
        return not_found(env, b"Launcher class not found.");
    }
    let run_launcher_mid = env.get_static_method_id(launcher_class, c"runLauncher", c"([[B[[BIJZ)V");
    if run_launcher_mid.is_null() {
        return not_found(env, b"Launcher entry point not found.");
    }
    let vm_args_fid = env.get_field_id(relaunch_exception_class, c"vmArgs", c"[Ljava/lang/String;");
    if vm_args_fid.is_null() {
        return not_found(env, b"RelaunchException vm args field not found.");
    }

    /* create args string array, without the program name */
    let argc = std::cmp::max(argc_native - 1, 0) as usize;
    let argv = (0..argc).map(|i| bytes(*argv_native.add(i + 1)));
    let Some(args) = new_byte_array_array(env, byte_array_class, argc, argv) else {
        return -1;
    };

    /* create env var args string array */
    let option_vars = option_vars_args.iter().map(|arg| arg.as_slice());
    let Some(option_vars_args_array) = new_byte_array_array(env, byte_array_class, option_vars_args.len(), option_vars) else {
        return -1;
    };

    /* invoke launcher entry point */
    let argv_native_long = argv_native as usize as jni::JLong;
    env.call_static_void_method(
        launcher_class,
        run_launcher_mid,
        &[
            JValue { l: option_vars_args_array },
            JValue { l: args },
            JValue { i: argc_native },
            JValue { j: argv_native_long },
            JValue { z: relaunch as jni::JBoolean },
        ],
    );
    let t = env.exception_occurred();
    if !t.is_null() {
        if env.is_instance_of(t, relaunch_exception_class) {
            if debug() {
                out(&[b"Relaunch exception has been thrown"]);
            }
            env.exception_clear();
            // read correct VM arguments from exception
            let vm_args = env.get_object_field(t, vm_args_fid);
            if exception_in(env, b"GetObjectField") {
                return -1;
            }
            let vm_arg_count = env.get_array_length(vm_args);
            if exception_in(env, b"GetArrayLength") {
                return -1;
            }
            if debug() {
                out(&[b"Relaunch VM arguments read: ", &num(vm_arg_count)]);
            }
            if !set_env(b"GRAALVM_LANGUAGE_LAUNCHER_VMARGS", &num(vm_arg_count)) {
                return -1;
            }
            for i in 0..vm_arg_count {
                let vm_arg_string = env.get_object_array_element(vm_args, i);
                if exception_in(env, b"GetObjectArrayElement") {
                    return -1;
                }
                let vm_arg = env.get_string_utf_chars(vm_arg_string);
                if exception_in(env, b"GetStringUTFChars") {
                    return -1;
                }
                /* set environment variables for relaunch vm arguments */
                let key = concat(&[b"GRAALVM_LANGUAGE_LAUNCHER_VMARGS_", &num(i)]);
                if !set_env(&key, bytes(vm_arg)) {
                    return -1;
                }
            }
            /* relaunch with correct VM arguments */
            return relaunch_executable(argv_native);
        }
        env.exception_describe();
        return -1;
    }
    0
}

/// Runs this executable again with the same arguments and the current environment.
unsafe fn relaunch_executable(argv_native: *mut *mut c_char) -> c_int {
    let path = c(&exe_path());
    #[cfg(unix)]
    {
        sys::execv(path.as_ptr(), argv_native);
        /* if we reach here, execv failed for sure */
        perror(c"execve failed".as_ptr());
        -1
    }
    #[cfg(windows)]
    {
        /* There is no way to replace the current process image on Windows: the C runtime
         * emulates exec*() by spawning a new process and exiting, which makes a parent process
         * waiting for the launcher believe it is done. So wait for the new process instead and
         * exit with its status. */
        let status = sys::_spawnv(sys::P_WAIT, path.as_ptr(), argv_native);
        if status == -1 {
            perror(c"execve failed".as_ptr());
            return -1;
        }
        exit(status as c_int)
    }
}
