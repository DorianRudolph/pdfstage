use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about = "Minimal MuPDF/WGPU PDF presentation viewer")]
pub(crate) struct Args {
    pub(crate) pdf: PathBuf,

    #[arg(short, long, help = "Open a second mirror window for screensharing")]
    pub(crate) mirror: bool,

    #[arg(short = 'r', long, help = "Poll the PDF and reload it when it changes")]
    pub(crate) hot_reload: bool,

    #[arg(
        short = 'f',
        long,
        help = "Allow windows to be resized without preserving slide aspect"
    )]
    pub(crate) free_aspect: bool,

    #[arg(
        short = 'c',
        long,
        default_value_t = 1024,
        help = "Per-window GPU page cache budget in MiB"
    )]
    pub(crate) cache_mib: u64,

    #[arg(
        short = 'a',
        long,
        default_value_t = 3,
        help = "Pages to render ahead of the current page"
    )]
    pub(crate) ahead: i32,
}
