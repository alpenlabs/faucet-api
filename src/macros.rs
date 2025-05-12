//! Macros for error handling.

#[macro_export]
macro_rules! err {
    ($item:expr) => {{
        ::core::result::Result::Err(::terrors::OneOf::new($item))
    }};
}

/// Displays a custom error message for faucet error type.
#[macro_export]
macro_rules! display_err {
    ($name:ident, $msg:expr) => {
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, $msg)
            }
        }
    };
}
