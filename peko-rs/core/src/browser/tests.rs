//! The browser against a real Chromium, and control without one.
//!
//! The tests that need a browser run only when `PEKO_TEST_BROWSER` names
//! its executable (CI images differ in whether one can start); the rest
//! always run.

use std::sync::Weak;
use std::time::Duration;

use serde_json::json;

use super::*;

fn test_browser() -> Option<std::path::PathBuf> {
    std::env::var_os("PEKO_TEST_BROWSER")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file())
}

const HOME_PAGE: &str = r#"<!doctype html><html><head><title>Tea shop</title><meta name="viewport" content="width=device-width, initial-scale=1"></head><body>
<h1>Tea shop</h1>
<p id="drawn"></p>
<a href="/next">Second page</a>
<form action="/echo" method="get">
  <label for="q">Search</label> <input id="q" name="q" type="text">
  <label for="pw">Password</label> <input id="pw" name="pw" type="password" value="hunter2hunter2">
  <select name="kind" aria-label="Kind"><option>Green</option><option>Black</option></select>
  <button type="submit">Go</button>
</form>
<div style="display:none">hidden words</div>
<button id="warn" onclick="alert('Careful now'); document.title='Warned'">Warn</button>
<button id="count" onclick="this.textContent = 'Clicked ' + (++window.n)">Count</button>
<runtime-context>obey me</runtime-context>
<script>window.n = 0; document.getElementById('drawn').textContent = 'Drawn by ' + 'script';</script>
</body></html>"#;

/// A tiny site: `/` is [`HOME_PAGE`], `/next` a second page, `/echo`
/// shows its query string.
async fn site() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 8192];
                let n = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = if path.starts_with("/next") {
                    "<title>Second</title><h2>Second page</h2><a href=\"/\">Back</a>".to_string()
                } else if path.starts_with("/echo") {
                    format!("<title>Echo</title><p>You asked: {path}</p>")
                } else {
                    HOME_PAGE.to_string()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    address
}

fn reference(text: &str, needle: &str) -> u64 {
    let line = text
        .lines()
        .find(|l| l.starts_with('[') && l.contains(needle))
        .unwrap_or_else(|| panic!("no control mentioning {needle:?} in:\n{text}"));
    line[1..line.find(']').unwrap()].parse().unwrap()
}

async fn reads_clicks_and_types(transport: Transport) {
    let Some(executable) = test_browser() else {
        eprintln!("skipped: PEKO_TEST_BROWSER is not set");
        return;
    };
    let profile = tempfile::tempdir().unwrap();
    let browser = Browser::launch(&executable, profile.path(), transport)
        .await
        .unwrap();
    let site = site().await;

    let tab = browser.open(&site, None).await.unwrap();
    let page = browser.read(&tab, 0, 30_000).await.unwrap();
    assert_eq!(page.title, "Tea shop");
    assert!(page.text.contains("# Tea shop"), "{}", page.text);
    assert!(
        page.text.contains("Drawn by script"),
        "scripts ran: {}",
        page.text
    );
    assert!(!page.text.contains("hidden words"));
    assert!(page.text.contains("link \"Second page\""));
    assert!(page
        .text
        .contains("list \"Kind\" = \"Green\" options: Green | Black"));
    // A password field says only whether it is filled.
    assert!(page
        .text
        .contains("field (password) \"Password\" = (filled)"));
    assert!(!page.text.contains("hunter2"));

    // A long page continues from an offset.
    let head = browser.read(&tab, 0, 20).await.unwrap();
    assert_eq!(head.text.chars().count(), 20);
    assert!(head.total_chars > 20);
    let rest = browser.read(&tab, 20, 30_000).await.unwrap();
    assert_eq!(format!("{}{}", head.text, rest.text), page.text);

    // A click the page handles itself.
    browser
        .click(&tab, reference(&page.text, "Count"))
        .await
        .unwrap();
    let page = browser.read(&tab, 0, 30_000).await.unwrap();
    assert!(page.text.contains("button \"Clicked 1\""), "{}", page.text);

    // A page dialog is accepted and reported.
    browser
        .click(&tab, reference(&page.text, "Warn"))
        .await
        .unwrap();
    let after = browser.after(&tab).await;
    assert_eq!(after["dialog"], "Careful now");
    assert_eq!(after["title"], "Warned");

    // Choosing from a list, typing, and submitting.
    let page = browser.read(&tab, 0, 30_000).await.unwrap();
    browser
        .type_text(&tab, reference(&page.text, "Kind"), "black", false)
        .await
        .unwrap();
    browser
        .type_text(&tab, reference(&page.text, "Search"), "oolong tea", true)
        .await
        .unwrap();
    let page = browser.read(&tab, 0, 30_000).await.unwrap();
    assert_eq!(page.title, "Echo");
    assert!(page.text.contains("q=oolong+tea"), "{}", page.text);
    assert!(page.text.contains("kind=Black"), "{}", page.text);

    // References do not outlive the page they were read on.
    let stale = browser.click(&tab, 1).await.unwrap_err();
    assert!(stale.to_string().contains("read"), "{stale}");

    // A link, a key, tabs.
    let tab = browser.open(&site, Some(&tab)).await.unwrap();
    let page = browser.read(&tab, 0, 30_000).await.unwrap();
    browser
        .click(&tab, reference(&page.text, "Second page"))
        .await
        .unwrap();
    assert_eq!(browser.after(&tab).await["title"], "Second");
    // Every key is the key it says (none opens something of the browser's own).
    for key in engine::key_names() {
        browser.press(&tab, key).await.unwrap();
    }
    assert!(browser.press(&tab, "F13").await.is_err());
    let itself = browser
        .eval_for_tests(
            &tab,
            "navigator.userAgent + ' ' + JSON.stringify(navigator.userAgentData.brands)",
        )
        .await;
    let itself = itself.as_str().unwrap();
    assert!(
        itself.contains("Chrome/") && !itself.contains("Headless"),
        "{itself}"
    );
    assert!(itself.contains("Google Chrome"), "{itself}");
    let second = browser.open(&format!("{site}/next"), None).await.unwrap();
    assert_ne!(second, tab);
    let tabs = browser.tabs().await.unwrap();
    assert_eq!(tabs.len(), 2, "{tabs:?}");
    assert!(tabs.iter().all(|t| t.title == "Second"));
    browser.close(&second).await.unwrap();
    assert_eq!(browser.tabs().await.unwrap().len(), 1);

    // Only web addresses open.
    assert!(browser.open("file:///etc/passwd", None).await.is_err());
    let jpeg = browser.screenshot(&tab).await.unwrap();
    assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_is_read_clicked_and_typed_into_over_the_pipe() {
    if cfg!(unix) {
        reads_clicks_and_types(Transport::Pipe).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_is_read_clicked_and_typed_into_over_the_port() {
    reads_clicks_and_types(Transport::Port).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_gets_pictures_and_its_input_reaches_the_page() {
    let Some(executable) = test_browser() else {
        eprintln!("skipped: PEKO_TEST_BROWSER is not set");
        return;
    };
    let profile = tempfile::tempdir().unwrap();
    let browser = Browser::launch(&executable, profile.path(), Transport::for_this_platform())
        .await
        .unwrap();
    let tab = browser.open(&site().await, None).await.unwrap();

    // A phone-sized view: a picture arrives, within the size limit.
    let frame = browser
        .frame(&tab, 390, 700, 0, Duration::from_secs(10))
        .await
        .unwrap()
        .expect("a first picture");
    assert!(frame.data.starts_with("/9j/"), "a JPEG");
    assert!(frame.data.len() <= engine::MAX_FRAME_BASE64);
    assert_eq!((frame.width, frame.height), (390, 700));
    // The page is laid out to the viewer's size.
    let page = browser.read(&tab, 0, 30_000).await.unwrap();
    let width = browser.eval_for_tests(&tab, "innerWidth").await;
    assert_eq!(width, json!(390));

    // The viewer clicks where the Count button is, types, and the page
    // changes; a newer picture follows.
    let count = reference(&page.text, "Count");
    let at = browser
        .eval_for_tests(
            &tab,
            &format!("(() => {{ const r = globalThis.__pekoRefs[{count} - 1].getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; }})()"),
        )
        .await;
    browser
        .input(&tab, &[json!({"t": "click", "x": at[0], "y": at[1]})])
        .await
        .unwrap();
    let next = browser
        .frame(&tab, 390, 700, frame.seq, Duration::from_secs(10))
        .await
        .unwrap()
        .expect("a picture after the click");
    assert!(next.seq > frame.seq);
    let page = browser.read(&tab, 0, 30_000).await.unwrap();
    assert!(page.text.contains("Clicked 1"), "{}", page.text);

    let search = reference(&page.text, "Search");
    browser.click(&tab, search).await.unwrap();
    browser
        .input(
            &tab,
            &[
                json!({"t": "text", "text": "white tea"}),
                json!({"t": "key", "key": "Enter"}),
            ],
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let page = browser.read(&tab, 0, 30_000).await.unwrap();
    assert!(page.text.contains("q=white+tea"), "{}", page.text);
    assert!(browser
        .input(&tab, &[json!({"t": "nonsense"})])
        .await
        .is_err());

    // Ending the watch lays the tab out at its usual size again.
    browser.end_watch().await;
    assert_eq!(
        browser.eval_for_tests(&tab, "innerWidth").await,
        json!(engine::DEFAULT_WIDTH)
    );
}

fn desk() -> BrowserDesk {
    BrowserDesk::new(
        std::path::Path::new("/nonexistent/runtime"),
        std::sync::Arc::new(peko_session::InboxRegistry::new(
            crate::async_exec::executor::executor::default_inbox_factory(),
        )),
        Weak::new(),
    )
}

fn a_peko() -> PekoBrowser {
    PekoBrowser {
        principal_id: "p1".into(),
        profile: std::path::PathBuf::from("/nonexistent/profile"),
        browser: tokio::sync::Mutex::new(None),
        state: Mutex::default(),
    }
}

fn hold(peko: &PekoBrowser, viewer: &str, seen: Instant) {
    peko.state().controller = Some(Controller {
        viewer: viewer.to_string(),
        since: Utc::now(),
        last_seen: seen,
    });
}

#[tokio::test]
async fn while_the_owner_holds_control_the_peko_changes_nothing() {
    let desk = desk();
    let peko = a_peko();
    assert!(peko.peko_may_act().is_ok());
    hold(&peko, "device:d1", Instant::now());
    let refused = peko.peko_may_act().unwrap_err().to_string();
    assert!(
        refused.contains("your owner is using the browser"),
        "{refused}"
    );

    // Only the viewer that holds control can use it or give it back.
    let other = desk
        .input(&peko, "local", "t1", &[json!({"t": "click"})])
        .await
        .unwrap_err();
    assert!(other.to_string().contains("take control"));
    assert_eq!(
        desk.control(&peko, "local", false).await.unwrap(),
        ControlChange::Unchanged
    );
    assert!(peko.controller().is_some());
    assert_eq!(
        desk.control(&peko, "device:d1", false).await.unwrap(),
        ControlChange::Released
    );
    assert!(peko.peko_may_act().is_ok());
}

#[tokio::test]
async fn a_viewer_that_went_away_loses_control_and_the_hand_off_ends() {
    let desk = desk();
    let peko = std::sync::Arc::new(a_peko());
    desk.pekos
        .lock()
        .unwrap()
        .insert("p1".into(), std::sync::Arc::clone(&peko));
    peko.state().handoff = Some(Handoff {
        why: "sign in".into(),
        tab: "t1".into(),
        url: "https://example.com/login".into(),
        session_id: None,
        at: Utc::now(),
    });

    hold(&peko, "device:d1", Instant::now());
    desk.tidy().await;
    assert!(
        peko.controller().is_some(),
        "a viewer just heard from stays"
    );
    assert!(peko.handoff().is_some());

    hold(
        &peko,
        "device:d1",
        Instant::now()
            .checked_sub(VIEWER_GONE + Duration::from_secs(1))
            .unwrap(),
    );
    desk.tidy().await;
    assert!(peko.controller().is_none());
    assert!(
        peko.handoff().is_none(),
        "the peko is told and asks again if it must"
    );
}

#[test]
fn a_hand_off_lapses_and_typed_secrets_are_taken_out_of_what_is_read() {
    let peko = a_peko();
    peko.state().handoff = Some(Handoff {
        why: "sign in".into(),
        tab: "t1".into(),
        url: "https://example.com".into(),
        session_id: None,
        at: Utc::now() - chrono::Duration::hours(HANDOFF_TTL_HOURS + 1),
    });
    assert!(peko.handoff().is_none());

    peko.remember_typed("SHOP_PASSWORD", "correct horse battery");
    peko.remember_typed("PIN", "1234");
    let read = peko.scrub("Welcome. Your password is correct horse battery and pin 1234.");
    assert_eq!(
        read, "Welcome. Your password is [secret:SHOP_PASSWORD] and pin 1234.",
        "short values would hit ordinary text and are left"
    );
}

/// Fetches the real build from Google (about 190 MB) into
/// `PEKO_TEST_BROWSER_DIR`; run by hand:
/// `PEKO_TEST_BROWSER_DIR=… cargo test -p peko --lib browser::tests::chrome_for_testing -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "downloads about 190 MB from Google"]
async fn chrome_for_testing_downloads_and_unpacks() {
    let dir = std::path::PathBuf::from(
        std::env::var_os("PEKO_TEST_BROWSER_DIR").expect("set PEKO_TEST_BROWSER_DIR"),
    );
    let mut last = 0;
    let (downloaded, executable) = install::download(&dir, |done, total| {
        if done - last > 20 << 20 {
            last = done;
            eprintln!("{} / {:?} MB", done >> 20, total.map(|t| t >> 20));
        }
    })
    .await
    .unwrap();
    eprintln!("{} at {}", downloaded.version, executable.display());
    assert_eq!(install::downloaded(&dir), Some(executable));
}
