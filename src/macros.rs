//! Macros for error handling.

#[macro_export]
macro_rules! err {
    ($item:expr) => {{
        ::core::result::Result::Err(::terrors::OneOf::new($item))
    }};
}
