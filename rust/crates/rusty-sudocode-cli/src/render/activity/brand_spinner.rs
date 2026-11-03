//! Fixed four-cell, one-row approximation of the Sudoprivacy mark.
//! Rasterized offline from sudowork's sudoclaw_transparent_large.png alpha at
//! twelve clockwise 30-degree steps. No image/font work runs on render ticks.

const FRAMES: [&str; 12] = [
    "⠰⢾⠶⠀",
    "⠰⢿⠷⠂",
    "⠰⢿⠷⠆",
    "⠐⠿⠿⠂",
    "⠰⠾⡿⠆",
    "⠐⠾⠿⠆",
    "⠀⠶⡷⠆",
    "⠠⢶⣷⠆",
    "⠰⢶⣷⠆",
    "⠠⣶⣶⠄",
    "⠰⣾⡶⠆",
    "⠰⣶⡶⠄",
];

#[inline]
pub(crate) fn frame(index: usize) -> &'static str {
    FRAMES[index % FRAMES.len()]
}
