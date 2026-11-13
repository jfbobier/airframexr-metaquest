mod passthrough_fb;

pub use passthrough_fb::*;
use openxr::{self as xr, sys};

fn xr_res(result: sys::Result) -> xr::Result<()> {
    if result.into_raw() >= 0 {
        Ok(())
    } else {
        Err(result)
    }
}
