use super::output::{print_value, resolve};
use super::transport;
use crate::ipc::Request;
use anyhow::Result;

pub fn cmd_avatars(
    username: Option<String>,
    out: Option<String>,
    limit: Option<usize>,
    json: bool,
) -> Result<()> {
    let resp = transport::send(Request::Avatars {
        username,
        out,
        limit,
    })?;
    print_value(&resp.data, &resolve(json))
}
