use crate::extra_extensions::PassthroughFB;
use alvr_common::anyhow::{bail, Result};
use alvr_system_info::Platform;
use openxr::{self as xr};
use std::{marker::PhantomData, ops::Deref, ptr};

pub struct PassthroughLayer<'a> {
    handle_fb: PassthroughFB,
    _marker: PhantomData<&'a ()>,
}

impl PassthroughLayer<'_> {
    pub fn new(session: &xr::Session<xr::Vulkan>, platform: Platform) -> Result<Self> {
        if session.instance().exts().fb_passthrough.is_none() {
            bail!("No passthrough extension available");
        }

        Ok(Self {
            handle_fb: PassthroughFB::new(session, platform)?,
            _marker: PhantomData,
        })
    }
}

impl<'a> Deref for PassthroughLayer<'a> {
    type Target = xr::CompositionLayerBase<'a, xr::Vulkan>;

    fn deref(&self) -> &Self::Target {
        unsafe { &*ptr::from_ref(self.handle_fb.layer()).cast() }
    }
}
