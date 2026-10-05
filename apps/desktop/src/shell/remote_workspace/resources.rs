//! Resource listing state, shared by the host entity and the workspace UI.

#[derive(Clone, Copy)]
pub(crate) enum Resource { Projects, Sessions }

#[derive(Clone)]
pub(crate) enum LoadState { Loading, Ready, #[allow(dead_code)] Failed(String) }
