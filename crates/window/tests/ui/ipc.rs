//! The bridge's sender checks end to end: only the bridge script, in the main frame of a page
//! on the daemon origin, reaches the daemon. A subframe on the daemon origin, a page on
//! another origin, a malformed or oversized message and an unknown method are dropped before
//! anything is forwarded or answered (`checks/ipc.js` posts them).

use std::fs;

use nsv_domain::helper::View;
use serde_json::json;

use crate::common::TOKEN;
use crate::site::{Content, Site};
use crate::{SERIAL, check_once, data_directory};

const PAGE: &str = r#"<!doctype html><meta charset="utf-8"><title>ipc</title><body></body>"#;

#[test]
#[expect(
    clippy::print_stderr,
    reason = "whether WebKit loaded the off-origin frame is worth seeing"
)]
fn only_the_bridge_script_on_the_daemon_page_reaches_the_daemon() {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let pages = data_directory("ipc-pages");
    fs::write(pages.join("index.html"), PAGE).unwrap();
    fs::write(pages.join("frame.html"), PAGE).unwrap();
    let site = Site::start(Content::Files(pages.clone(), "index.html"), "ipc");
    // The same server under another host name is another origin.
    let elsewhere = format!("http://localhost:{}", site.port);
    site.check_with("ipc", &json!({ "elsewhere": elsewhere }));
    let result = check_once("ipc", &site, View::Main { profile: None }, TOKEN);
    eprintln!("the off-origin frame: {}", result["offOriginFrame"]);
    fs::remove_dir_all(&pages).unwrap_or_default();
}
