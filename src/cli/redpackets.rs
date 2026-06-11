use super::output::{print_value, resolve};
use super::transport;
use crate::ipc::Request;
use anyhow::Result;

pub fn cmd_redpackets(limit: Option<usize>, json: bool) -> Result<()> {
    let resp = transport::send(Request::Redpackets { limit })?;
    print_value(&resp.data, &resolve(json))
}
