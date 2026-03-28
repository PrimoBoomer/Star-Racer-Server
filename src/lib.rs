#![forbid(unsafe_code)]

pub mod error;
pub mod lobby;
pub mod protocol;
pub mod run;

pub type Result<T> = std::result::Result<T, error::Error>;

#[macro_export]
macro_rules! sr_log {
    ( $level:ident, $section:expr, $fmt:expr $(, $arg:expr)*) => {
        {
            use colored::Colorize;
            use std::hash::{DefaultHasher, Hash, Hasher};
            let level_str = stringify!($level);
            let color_level = match level_str {
                "trace" => (140, 140, 140),
                "debug" => (150, 172, 100),
                "info" => (240, 240, 240),
                "warn" => (237, 99, 0),
                "error" => (219, 9, 23),
                _ => (20, 20, 20)
            };
            let mut s = DefaultHasher::new();
            $section.to_string().hash(&mut s);
            let hash = s.finish();
            let r = (hash & 0xFF) as u8;
            let g = ((hash & 0xFF00) >> 8) as u8;
            let b = ((hash & 0xFF0000) >> 16) as u8;

            log::$level!("{:>15}|{}", $section.to_string().truecolor(r, g, b),
                format!($fmt, $(
                    $arg,
                )*).truecolor(color_level.0, color_level.1, color_level.2)
            )
        }
    }
}
