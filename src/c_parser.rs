use std::{ffi::{c_char, CStr}, str::FromStr};

pub fn str_from_c_char(raw: *const c_char) -> String {
    let cstr = unsafe { CStr::from_ptr(raw) };

    String::from_utf8_lossy(cstr.to_bytes()).to_string()
}

pub fn as_string(raw: *const c_char) -> Option<String> {
    Some(str_from_c_char(raw))
}

pub fn as_from_str<F: FromStr>(raw: *const c_char) -> Option<F> {
    let s = str_from_c_char(raw);

    let res = s.parse::<F>();
    if let Ok(val) = res {
        Some(val)
    } else {
        None
    }
}
