//! Driving headless Chromium from tokio tests. rustwright's API blocks and
//! runs its own runtime, so a journey runs on a blocking thread while the
//! server keeps the test runtime's worker threads.

use std::time::{Duration, Instant};

use rustwright::{chromium, ActionOptions, Browser, GotoOptions, LaunchOptions, Page};

use crate::browser_home::BrowserHome;

/// How long `wait_*` polls before failing. A generous upper bound, not a
/// pace: a poll returns the moment its condition holds, so a passing journey
/// never waits this long, and a machine running several gates at once (load
/// average past 16, issue #500) gets the time a page needs to draw.
const WAIT: Duration = Duration::from_secs(60);

/// Chromium's rule that `localhost` is the IPv4 loopback, where the test
/// servers listen. Without it Chromium tries `::1` first, and the servers
/// bind `127.0.0.1` on a port the OS picks with only IPv4 in mind: the same
/// port number can be an IPv6-only listener of another process (a container
/// engine's port proxy listens on `[::]` for every port it publishes), which
/// then answers the browser with an empty response or a reset instead of
/// the connection being refused and retried on IPv4 (issue #500, seen as
/// `ERR_EMPTY_RESPONSE` on the sign-in page). The page's origin stays
/// `localhost`, so its secure-context and WebAuthn behaviour are unchanged.
const LOCALHOST_IS_IPV4: &str = "--host-resolver-rules=MAP localhost 127.0.0.1";

/// Lets Chromium choose its own DevTools port (issue #1101).
///
/// Without this argument rustwright picks a free port itself
/// (`pick_unused_port`: bind port 0, read the number, close the socket) and
/// passes it to Chromium, so a parallel test process's Chromium can take the
/// port in between ("bind() failed: Address already in use"). With port 0
/// rustwright's launcher (`launch_chromium_attempt`, `dynamic_debugging_port`)
/// does not poll a port it chose: it waits for Chromium to write
/// `DevToolsActivePort` into the profile folder, which holds the port the
/// browser actually bound, and builds the endpoint from that file.
const CHROMIUM_CHOOSES_DEBUGGING_PORT: &str = "--remote-debugging-port=0";

/// The launch options for a browser living in `home`: headless, with the
/// harness's own profile folder and launcher, and no port chosen by the
/// harness or by rustwright.
fn launch_options(home: &BrowserHome) -> LaunchOptions {
    let mut options = LaunchOptions::default()
        .headless(true)
        .arg(LOCALHOST_IS_IPV4)
        .arg(CHROMIUM_CHOOSES_DEBUGGING_PORT);
    options.user_data_dir = Some(home.profile().to_string_lossy().into_owned());
    options.executable_path = Some(home.launcher().to_string_lossy().into_owned());
    options
}

/// Holds the page's next request with method `arg.method` to a path ending
/// in `arg.suffix` until `window.releaseHeld()`; `window.heldDone` turns
/// true once that held answer has been handed back to the page, and
/// `window.heldMatches` counts the matching requests the page made while it
/// was held (the held one included). With `arg.early`, the request is sent
/// at once and only its answer is held (`window.heldFetched` turns true
/// when the server has answered): a read that finishes late with what it
/// read early. Runs through `ui.eval`, so it wraps `fetch` without touching
/// the page's code.
const HOLD_NEXT: &str = "(arg) => {
    const original = window.fetch;
    let release = null;
    window.heldDone = false;
    window.heldFetched = false;
    window.heldMatches = 0;
    window.releaseHeld = () => release();
    window.fetch = (input, init) => {
        const method = (init && init.method) || 'GET';
        const matches = method === arg.method && String(input).endsWith(arg.suffix);
        if (matches) window.heldMatches += 1;
        if (release === null && matches) {
            const held = new Promise(resolve => { release = resolve; });
            if (arg.early) {
                const answer = original(input, init);
                const fetched = () => { window.heldFetched = true; };
                answer.then(fetched, fetched);
                return held.then(() => answer)
                    .finally(() => { window.heldDone = true; window.fetch = original; });
            }
            return held.then(() => original(input, init))
                .finally(() => { window.heldDone = true; window.fetch = original; });
        }
        return original(input, init);
    };
    return true;
}";

/// Answers the page's next request with method `arg.method` to a path
/// ending in `arg.suffix` with status `arg.status` and the error body
/// `{error: arg.kind ?? 'test', message: arg.message}`, without reaching
/// the server (a refusal
/// or an outage the console must survive). Later requests go through. The
/// wrapper stays installed after it has answered (passing everything on),
/// so several of them nest: each answers its own request once.
const FAIL_NEXT: &str = "(arg) => {
    const original = window.fetch;
    let used = false;
    window.fetch = (input, init) => {
        const method = (init && init.method) || 'GET';
        if (!used && method === arg.method && String(input).endsWith(arg.suffix)) {
            used = true;
            return Promise.resolve(new Response(
                JSON.stringify({ error: arg.kind ?? 'test', message: arg.message, trace_id: arg.trace_id }),
                { status: arg.status, headers: { 'Content-Type': 'application/json' } }));
        }
        return original(input, init);
    };
    return true;
}";

/// One browser page on the console.
pub struct Ui {
    /// The server, `http://127.0.0.1:<port>`.
    pub base: String,
    /// The page.
    pub page: Page,
    // Declared after `page`, so the page is dropped before its browser
    // (Rust drops struct fields in declaration order).
    browser: Browser,
    // Declared last, so the folder is removed after the browser has been
    // closed (issue #1070, `browser_home`).
    home: BrowserHome,
}

/// Runs `journey` in a fresh headless Chromium, then fails it on any console
/// error (script errors and CSP violations are logged as errors by main.js).
/// Returns without running when no Chromium is installed.
///
/// # Returns
/// Whether the journey ran: `false` when it was skipped. A test that checks
/// server state the journey changed returns early on `false`, so it skips
/// too rather than failing on the unchanged state.
pub async fn drive<F>(base: String, journey: F) -> bool
where
    F: FnOnce(&Ui) + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let Some(chromium_binary) = chromium().executable_path() else {
            eprintln!("skipping browser journey: no Chromium found (set RUSTWRIGHT_CHROMIUM)");
            return false;
        };
        // The harness owns the profile folder and the launcher (issue
        // #1070): rustwright's own profile folder comes back after it is
        // deleted, and its browser outlives a test killed before `Drop`.
        let home = BrowserHome::create(&chromium_binary).expect("create the browser's folder");
        let browser = chromium()
            .launch(launch_options(&home))
            .expect("launch Chromium");
        let page = browser.new_page().expect("open a page");
        page.arm_console_capture().expect("capture the console");
        let ui = Ui {
            base,
            page,
            browser,
            home,
        };
        journey(&ui);
        ui.assert_no_console_errors();
        true
    })
    .await
    .expect("the journey thread panicked")
}

impl Ui {
    /// The folder holding this browser's profile and launcher, deleted when
    /// the journey ends.
    pub fn home_path(&self) -> &std::path::Path {
        self.home.path()
    }

    /// Navigates to `#<route>` (route like "/keys"; "" for the shell).
    pub fn open(&self, route: &str) {
        self.page
            .goto(
                &format!("{}/ui/#{route}", self.base),
                GotoOptions::default(),
            )
            .expect("navigate");
    }

    /// Signs in with `key` from the login card and waits for the shell.
    pub fn login(&self, key: &str) {
        self.open("");
        self.wait_visible("#login");
        self.fill("#login-form input[name=key]", key);
        self.click("#login-form button[type=submit]");
        self.wait_visible("#app");
    }

    /// Signs in on `/signin` with an email and password, and waits for the
    /// console to show who is signed in (its content, not only the shell:
    /// the shell is visible before `whoami` answers).
    pub fn sign_in_password(&self, email: &str, password: &str) {
        self.page
            .goto(&format!("{}/signin", self.base), GotoOptions::default())
            .expect("open the sign-in page");
        self.fill("#signin-email", email);
        self.fill("#signin-password", password);
        self.click("#signin-submit");
        self.wait_visible("#app");
        // `#whoami` reads "<user> - <level>" once the identity is loaded.
        self.wait_text("#whoami", " - ");
    }

    /// Navigates to the reader at `#<route>` (route like "/v/main/a.md";
    /// "" for its home).
    pub fn open_reader(&self, route: &str) {
        self.page
            .goto(
                &format!("{}/read/#{route}", self.base),
                GotoOptions::default(),
            )
            .expect("navigate to the reader");
        // A hash-only navigation commits a moment after `goto` returns; a
        // reload sent in that gap would reload the previous route. Give the
        // hash a short time to settle (the reader may rewrite it, so a
        // different hash is not an error).
        let wanted = format!("#{route}");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline
            && self
                .page
                .evaluate("() => location.hash", None, ActionOptions::default())
                .ok()
                != Some(serde_json::json!(wanted))
        {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Opens the reader at `#<route>` signed out, follows the signed-out
    /// screen's "Sign in" to the sign-in page, signs in there, and waits
    /// until the reader is back and shown.
    pub fn sign_in_reader(&self, route: &str, email: &str, password: &str) {
        self.open_reader(route);
        self.wait_visible("#signed-out-signin");
        self.click("#signed-out-signin");
        self.wait_visible("#signin-email");
        self.fill("#signin-email", email);
        self.fill("#signin-password", password);
        self.click("#signin-submit");
        self.wait_visible("#reader");
    }

    /// Waits until the reader's service worker controls the page.
    pub fn wait_worker(&self) {
        self.poll("the reader's service worker", || {
            self.eval(
                "() => navigator.serviceWorker.controller !== null",
                serde_json::json!(null),
            ) == serde_json::json!(true)
        });
    }

    /// Cuts the page and every service worker off the network (DevTools
    /// network emulation on each target) while the returned value lives.
    /// Dropping it sets `offline: false` on each target it changed before
    /// the connection closes: closing alone does not reliably end the
    /// emulation on a service-worker target.
    ///
    /// # Panics
    /// If Chromium refuses a DevTools call.
    pub fn go_offline(&self) -> DevTools {
        let mut devtools = DevTools::connect(&self.browser);
        let targets = devtools.call(None, "Target.getTargets", serde_json::json!({}));
        let ids: Vec<String> = targets["targetInfos"]
            .as_array()
            .expect("targets")
            .iter()
            .filter(|target| target["type"] == "page" || target["type"] == "service_worker")
            .filter_map(|target| target["targetId"].as_str().map(str::to_string))
            .collect();
        for id in ids {
            let attached = devtools.call(
                None,
                "Target.attachToTarget",
                serde_json::json!({ "targetId": id, "flatten": true }),
            );
            let session = attached["sessionId"]
                .as_str()
                .expect("a DevTools session")
                .to_string();
            devtools.call(Some(&session), "Network.enable", serde_json::json!({}));
            devtools.set_offline(&session, true);
            devtools.offline_sessions.push(session);
        }
        devtools
    }

    /// Sets this origin's storage quota to what it uses now (DevTools'
    /// `Storage.overrideQuotaForOrigin`), so every further cache write is
    /// refused with QuotaExceededError while the returned value lives.
    ///
    /// # Panics
    /// If Chromium refuses a DevTools call.
    pub fn limit_storage(&self) -> DevTools {
        let used = self
            .eval(
                "async () => (await navigator.storage.estimate()).usage",
                serde_json::json!(null),
            )
            .as_f64()
            .expect("storage usage");
        self.limit_storage_to(used)
    }

    /// [`Ui::limit_storage`] with the quota given in bytes. A quota below
    /// what is used now stays exceeded even after the reader deletes some
    /// of its copies, so a write of any size is refused.
    ///
    /// # Panics
    /// If Chromium refuses a DevTools call.
    pub fn limit_storage_to(&self, quota: f64) -> DevTools {
        let mut devtools = DevTools::connect(&self.browser);
        let attached = devtools.call(
            None,
            "Target.attachToTarget",
            serde_json::json!({ "targetId": self.page.target_id(), "flatten": true }),
        );
        let session = attached["sessionId"]
            .as_str()
            .expect("a DevTools session")
            .to_string();
        devtools.call(
            Some(&session),
            "Storage.overrideQuotaForOrigin",
            serde_json::json!({ "origin": self.base, "quotaSize": quota }),
        );
        devtools
    }

    /// Makes every `Cache.put` of a URL containing `needle` reject with
    /// QuotaExceededError in every running service worker, while the
    /// returned value lives (a full disk for that one write, while the
    /// others succeed). The override is in the worker's memory only, so it
    /// ends when that worker does.
    ///
    /// # Panics
    /// If Chromium refuses a DevTools call.
    pub fn refuse_cache_puts_of(&self, needle: &str) -> DevTools {
        let source = format!(
            "{{ const put = Cache.prototype.put; \
               Cache.prototype.put = function (request, response) {{ \
                 const url = typeof request === 'string' ? request : request.url; \
                 if (url.includes({needle:?})) return Promise.reject(new DOMException('full', 'QuotaExceededError')); \
                 return put.call(this, request, response); }}; }}"
        );
        let mut devtools = DevTools::connect(&self.browser);
        let targets = devtools.call(None, "Target.getTargets", serde_json::json!({}));
        let ids: Vec<String> = targets["targetInfos"]
            .as_array()
            .expect("targets")
            .iter()
            .filter(|target| target["type"] == "service_worker")
            .filter_map(|target| target["targetId"].as_str().map(str::to_string))
            .collect();
        for id in ids {
            let attached = devtools.call(
                None,
                "Target.attachToTarget",
                serde_json::json!({ "targetId": id, "flatten": true }),
            );
            let session = attached["sessionId"]
                .as_str()
                .expect("a DevTools session")
                .to_string();
            devtools.call(
                Some(&session),
                "Runtime.evaluate",
                serde_json::json!({ "expression": source }),
            );
        }
        devtools
    }

    /// Makes every Cache Storage call reject with SecurityError, in the
    /// page (now and on every later load) and in the reader's service
    /// worker, while the returned value lives.
    ///
    /// # Panics
    /// If Chromium refuses a DevTools call.
    pub fn deny_cache_storage(&self) -> DevTools {
        const DENY: &str = "for (const name of ['open', 'match', 'keys', 'delete', 'has']) { \
            CacheStorage.prototype[name] = function () { \
                return Promise.reject(new DOMException('denied', 'SecurityError')); }; }";
        let mut devtools = DevTools::connect(&self.browser);
        let targets = devtools.call(None, "Target.getTargets", serde_json::json!({}));
        let infos: Vec<(String, String)> = targets["targetInfos"]
            .as_array()
            .expect("targets")
            .iter()
            .filter_map(|target| {
                Some((
                    target["type"].as_str()?.to_string(),
                    target["targetId"].as_str()?.to_string(),
                ))
            })
            .filter(|(kind, _)| kind == "page" || kind == "service_worker")
            .collect();
        for (kind, id) in infos {
            let attached = devtools.call(
                None,
                "Target.attachToTarget",
                serde_json::json!({ "targetId": id, "flatten": true }),
            );
            let session = attached["sessionId"]
                .as_str()
                .expect("a DevTools session")
                .to_string();
            if kind == "page" {
                devtools.call(
                    Some(&session),
                    "Page.addScriptToEvaluateOnNewDocument",
                    serde_json::json!({ "source": DENY }),
                );
            }
            devtools.call(
                Some(&session),
                "Runtime.evaluate",
                serde_json::json!({ "expression": DENY }),
            );
        }
        devtools
    }

    /// Reloads the current page and waits for the reader to show again.
    pub fn reload_reader(&self) {
        self.page.reload(GotoOptions::default()).expect("reload");
        self.wait_visible("#reader");
    }

    fn poll(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}");
    }

    /// Waits until `selector` is visible.
    pub fn wait_visible(&self, selector: &str) {
        self.poll(selector, || self.page.is_visible(selector).unwrap_or(false));
    }

    /// Waits until `selector` is absent or hidden.
    pub fn wait_hidden(&self, selector: &str) {
        self.poll(selector, || {
            !self.page.is_visible(selector).unwrap_or(false)
        });
    }

    /// Waits until the checkbox `selector` is ticked.
    ///
    /// # Arguments
    /// * `selector` - the checkbox to watch.
    ///
    /// # Panics
    /// When it is still unticked after [`WAIT`].
    pub fn wait_checked(&self, selector: &str) {
        self.poll(&format!("{selector} ticked"), || {
            self.eval(
                "(selector) => document.querySelector(selector)?.checked === true",
                serde_json::json!(selector),
            ) == serde_json::json!(true)
        });
    }

    /// Waits until `selector`'s text contains `contains`, and returns the text.
    pub fn wait_text(&self, selector: &str, contains: &str) -> String {
        let mut last = String::new();
        self.poll(&format!("{selector} containing {contains:?}"), || {
            last = self.text(selector);
            last.contains(contains)
        });
        last
    }

    /// Waits until `selector`'s text no longer contains `gone` (an absent
    /// element has no text, so that counts too).
    ///
    /// # Arguments
    /// * `selector` - the element to watch.
    /// * `gone` - the text that must disappear.
    ///
    /// # Panics
    /// When the text is still there after [`WAIT`].
    pub fn wait_text_gone(&self, selector: &str, gone: &str) {
        self.poll(&format!("{selector} without {gone:?}"), || {
            !self.text(selector).contains(gone)
        });
    }

    /// `selector`'s text content, or "" when it is absent.
    pub fn text(&self, selector: &str) -> String {
        self.page
            .text_content(selector, ActionOptions::default())
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    /// Clicks `selector` (auto-waits for it).
    pub fn click(&self, selector: &str) {
        self.page
            .click(selector, ActionOptions::default())
            .expect("click");
    }

    /// Fills the input `selector` with `value` (auto-waits for it).
    pub fn fill(&self, selector: &str, value: &str) {
        self.page
            .fill(selector, value, ActionOptions::default())
            .expect("fill");
    }

    /// Chooses `value` in the `<select>` `selector`, firing its `input` and
    /// `change` events. rustwright's `fill` refuses a `<select>`, and its
    /// `select_options` does not wait, so this polls until the select
    /// exists and has an option of that value (pickers fill theirs after an
    /// API call).
    pub fn select(&self, selector: &str, value: &str) {
        self.poll(&format!("{selector} offering {value:?}"), || {
            self.page.select_options(selector, &[value]).is_ok()
        });
    }

    /// Runs a JS function expression `(arg) => ...` in the page and returns
    /// its JSON result. The function form runs under the console's CSP.
    pub fn eval(&self, function: &str, arg: serde_json::Value) -> serde_json::Value {
        self.page
            .evaluate(function, Some(&arg), ActionOptions::default())
            .expect("evaluate")
    }

    /// The `type` property of the input `selector` (`password` or `text`
    /// for a password field with a show/hide button), and its value.
    ///
    /// # Arguments
    /// * `selector` - a CSS selector for one input.
    ///
    /// # Returns
    /// `(type, value)`; empty strings when nothing matches.
    pub fn input_state(&self, selector: &str) -> (String, String) {
        let state = self.eval(
            "(selector) => { const el = document.querySelector(selector); \
                return el ? [el.type, el.value] : ['', '']; }",
            serde_json::json!(selector),
        );
        let part = |index: usize| state[index].as_str().unwrap_or_default().to_string();
        (part(0), part(1))
    }

    /// An attribute of the first element matching `selector`.
    ///
    /// # Arguments
    /// * `selector` - a CSS selector.
    /// * `name` - the attribute's name.
    ///
    /// # Returns
    /// Its value, or an empty string when absent.
    pub fn attribute(&self, selector: &str, name: &str) -> String {
        self.eval(
            "([selector, name]) => document.querySelector(selector)?.getAttribute(name) ?? ''",
            serde_json::json!([selector, name]),
        )
        .as_str()
        .unwrap_or_default()
        .to_string()
    }

    /// Whether an element matching `selector` is in the page now (no wait).
    ///
    /// # Arguments
    /// * `selector` - a CSS selector.
    pub fn present(&self, selector: &str) -> bool {
        self.eval(
            "(selector) => document.querySelector(selector) !== null",
            serde_json::json!(selector),
        ) == serde_json::json!(true)
    }

    /// Holds the page's next `method` request to a path ending in `suffix`
    /// until [`Ui::release_held`] (see [`HOLD_NEXT`]).
    ///
    /// # Arguments
    /// * `method` - the HTTP method, upper case.
    /// * `suffix` - the end of the request's URL.
    pub fn hold_next(&self, method: &str, suffix: &str) {
        self.eval(
            HOLD_NEXT,
            serde_json::json!({ "method": method, "suffix": suffix, "early": false }),
        );
    }

    /// Sends the page's next `method` request to a path ending in `suffix`
    /// at once but holds its answer until [`Ui::release_held`], then waits
    /// until the server has answered it (see [`HOLD_NEXT`]): the page gets,
    /// late, what the server said early.
    ///
    /// # Arguments
    /// * `method` - the HTTP method, upper case.
    /// * `suffix` - the end of the request's URL.
    /// * `trigger` - what makes the page send it, run after the hold is set.
    ///
    /// # Panics
    /// When the server has not answered within [`WAIT`].
    pub fn hold_answer(&self, method: &str, suffix: &str, trigger: impl FnOnce()) {
        self.eval(
            HOLD_NEXT,
            serde_json::json!({ "method": method, "suffix": suffix, "early": true }),
        );
        trigger();
        self.poll(&format!("the server's answer to {method} {suffix}"), || {
            self.eval("() => window.heldFetched", serde_json::json!(null))
                == serde_json::json!(true)
        });
    }

    /// How many matching requests the page made since [`Ui::hold_next`],
    /// the held one included.
    pub fn held_matches(&self) -> u64 {
        self.eval("() => window.heldMatches", serde_json::json!(null))
            .as_u64()
            .unwrap_or(0)
    }

    /// Releases the held answer, waits until the page has it, then gives
    /// the page a moment to act on it (a stale drawer would be drawn by
    /// then).
    ///
    /// # Panics
    /// When the held answer has not arrived within [`WAIT`].
    pub fn release_held(&self) {
        self.eval("() => window.releaseHeld()", serde_json::json!(null));
        let deadline = Instant::now() + WAIT;
        while self.eval("() => window.heldDone", serde_json::json!(null)) != serde_json::json!(true)
        {
            assert!(Instant::now() < deadline, "the held answer never arrived");
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    /// Makes the page's next `method` request to a path ending in `suffix`
    /// answer `status` with `message`, without reaching the server (see
    /// [`FAIL_NEXT`]).
    ///
    /// # Arguments
    /// * `method` - the HTTP method, upper case.
    /// * `suffix` - the end of the request's URL.
    /// * `status` - the status to answer, such as 409 or 503.
    /// * `message` - the error body's `message`.
    pub fn fail_next(&self, method: &str, suffix: &str, status: u16, message: &str) {
        self.eval(
            FAIL_NEXT,
            serde_json::json!({
                "method": method, "suffix": suffix, "status": status, "message": message,
            }),
        );
    }

    /// As [`Ui::fail_next`], with the error body's `trace_id` set, as an
    /// internal error's is (issue #1093).
    ///
    /// # Arguments
    /// * `method` - the HTTP method, upper case.
    /// * `suffix` - the end of the request's URL.
    /// * `status` - the status to answer.
    /// * `trace_id` - the error body's `trace_id`.
    pub fn fail_next_traced(&self, method: &str, suffix: &str, status: u16, trace_id: &str) {
        let armed = self.eval(
            FAIL_NEXT,
            serde_json::json!({
                "method": method, "suffix": suffix, "status": status,
                "message": "internal error; the server log has the details",
                "trace_id": trace_id,
            }),
        );
        assert_eq!(armed, serde_json::json!(true));
    }

    /// As [`Ui::fail_next`], with the error body's `error` kind set to
    /// `kind` (such as `UnlockRequired`), which decides how the page reads
    /// the refusal.
    ///
    /// # Arguments
    /// * `method` - the HTTP method, upper case.
    /// * `suffix` - the end of the request's URL.
    /// * `status` - the status to answer.
    /// * `kind` - the error body's `error`.
    /// * `message` - the error body's `message`.
    pub fn fail_next_kind(
        &self,
        method: &str,
        suffix: &str,
        status: u16,
        kind: &str,
        message: &str,
    ) {
        let armed = self.eval(
            FAIL_NEXT,
            serde_json::json!({
                "method": method, "suffix": suffix, "status": status,
                "kind": kind, "message": message,
            }),
        );
        assert_eq!(armed, serde_json::json!(true));
    }

    /// Waits until the JS function `function` (no argument, may be async)
    /// returns `true`.
    ///
    /// # Panics
    /// When it has not within [`WAIT`].
    pub fn poll_eval(&self, function: &str) {
        self.poll(function, || {
            self.eval(function, serde_json::json!(null)) == serde_json::json!(true)
        });
    }

    /// Sends the page's next `method` request to a path ending in `suffix`
    /// at once and holds only its answer in the page; `window.heldFetched`
    /// turns true when the answer arrives (see [`HOLD_NEXT`]). Unlike
    /// [`Ui::hold_answer`], it returns at once, so the answer can be held
    /// elsewhere too (in the worker, [`Ui::pause_worker_responses`]).
    ///
    /// # Arguments
    /// * `method` - the HTTP method, upper case.
    /// * `suffix` - the end of the request's URL.
    pub fn hold_answer_later(&self, method: &str, suffix: &str) {
        self.eval(
            HOLD_NEXT,
            serde_json::json!({ "method": method, "suffix": suffix, "early": true }),
        );
    }

    /// Pauses every response the reader's service worker gets for a URL
    /// matching `pattern` (DevTools wildcards), after the server answered
    /// and before the worker sees it (DevTools `Fetch` at the `Response`
    /// stage, on the `service_worker` target).
    ///
    /// # Panics
    /// When no service worker runs, or Chromium refuses a DevTools call.
    pub fn pause_worker_responses(&self, pattern: &str) -> PausedResponses {
        let mut devtools = DevTools::connect(&self.browser);
        let targets = devtools.call(None, "Target.getTargets", serde_json::json!({}));
        let worker = targets["targetInfos"]
            .as_array()
            .expect("targets")
            .iter()
            .find(|target| target["type"] == "service_worker")
            .and_then(|target| target["targetId"].as_str())
            .expect("the reader's service worker")
            .to_string();
        let attached = devtools.call(
            None,
            "Target.attachToTarget",
            serde_json::json!({ "targetId": worker, "flatten": true }),
        );
        let session = attached["sessionId"]
            .as_str()
            .expect("a DevTools session")
            .to_string();
        devtools.call(
            Some(&session),
            "Fetch.enable",
            serde_json::json!({ "patterns": [{ "urlPattern": pattern, "requestStage": "Response" }] }),
        );
        PausedResponses { devtools, session }
    }

    /// Runs `visit` on a second tab of the same browser (the default
    /// context, so the same cookies), and closes the tab after.
    pub fn in_second_tab(&self, visit: impl FnOnce(&Page)) {
        let tab = self.browser.new_page().expect("open a second tab");
        visit(&tab);
        // rustwright closes a page's target when the page is dropped.
        drop(tab);
    }

    /// While the returned guard lives, every service worker that starts
    /// gets `body` (as CSS) instead of the server's answer for URLs matching
    /// `pattern`: the files of another build. DevTools auto-attaches to each
    /// new service worker paused before its script runs, enables `Fetch` on
    /// it, then lets it run; a background thread answers the events.
    ///
    /// # Panics
    /// If Chromium refuses the auto-attach.
    pub fn tamper_new_workers(&self, pattern: &str, body: &str) -> Tamper {
        Tamper::start(DevTools::connect(&self.browser), pattern, body)
    }

    /// Clicks the element `selector` names twice in a row, in the page
    /// (a disabled button ignores both clicks).
    ///
    /// # Returns
    /// Whether the element is disabled afterwards.
    pub fn click_twice(&self, selector: &str) -> bool {
        self.eval(
            "(selector) => { const element = document.querySelector(selector); \
             element.click(); element.click(); return element.disabled; }",
            serde_json::json!(selector),
        ) == serde_json::json!(true)
    }

    /// Marks the element `selector` names now, so [`Ui::is_marked`] can tell
    /// whether the page has drawn a new one in its place since.
    pub fn mark(&self, selector: &str) {
        self.eval(
            "(selector) => { document.querySelector(selector).dataset.uiMarked = 'yes'; }",
            serde_json::json!(selector),
        );
    }

    /// Whether the element `selector` names now is the one [`Ui::mark`]
    /// marked (false once the page has drawn a new one).
    pub fn is_marked(&self, selector: &str) -> bool {
        self.eval(
            "(selector) => document.querySelector(selector)?.dataset.uiMarked === 'yes'",
            serde_json::json!(selector),
        ) == serde_json::json!(true)
    }

    /// Waits until the page has drawn a new element for `selector` in place
    /// of the one [`Ui::mark`] marked.
    ///
    /// # Panics
    /// When the marked one is still there after [`WAIT`].
    pub fn wait_redrawn(&self, selector: &str) {
        self.poll(&format!("{selector} drawn again"), || {
            !self.is_marked(selector)
        });
    }

    /// Clicks `selector` and waits for the file it downloads, as Chromium
    /// saves it. rustwright reports a download's name but cannot enable
    /// downloads from Rust (only its Python binding can), so this opens its
    /// own DevTools connection: `Browser.setDownloadBehavior` (with events)
    /// for the page's browser context into a fresh directory, then
    /// `Browser.downloadWillBegin` and `Browser.downloadProgress` until the
    /// file is complete.
    ///
    /// # Arguments
    /// * `selector` - the control that starts the download.
    ///
    /// # Returns
    /// The suggested file name and the file's text.
    ///
    /// # Panics
    /// When no download starts or completes within [`WAIT`], or a DevTools
    /// call fails.
    pub fn download(&self, selector: &str) -> (String, String) {
        let mut devtools = DevTools::connect(&self.browser);
        let target = devtools.call(
            None,
            "Target.getTargetInfo",
            serde_json::json!({ "targetId": self.page.target_id() }),
        );
        let directory = tempfile::tempdir().expect("a download directory");
        let mut behavior = serde_json::json!({
            "behavior": "allowAndName",
            "downloadPath": directory.path(),
            "eventsEnabled": true,
        });
        if let Some(context) = target["targetInfo"]["browserContextId"].as_str() {
            behavior["browserContextId"] = serde_json::json!(context);
        }
        // A context rustwright created belongs to its own connection: Chrome
        // answers "Failed to find browser context" to any other. Fall back to
        // the default context, as rustwright's own `enable_downloads` does.
        if let Err(error) = devtools.try_call(None, "Browser.setDownloadBehavior", behavior.clone())
        {
            assert!(
                error.to_string().contains("Failed to find browser context"),
                "Browser.setDownloadBehavior: {error}"
            );
            // `as_object_mut` borrows the JSON object mutably, like a dict.
            if let Some(params) = behavior.as_object_mut() {
                params.remove("browserContextId");
            }
            devtools.call(None, "Browser.setDownloadBehavior", behavior);
        }
        self.click(selector);
        let deadline = Instant::now() + WAIT;
        let begun = devtools.event("Browser.downloadWillBegin", deadline, |_| true);
        let guid = begun["guid"].as_str().expect("a download guid").to_string();
        let name = begun["suggestedFilename"]
            .as_str()
            .expect("a suggested name")
            .to_string();
        let done = devtools.event("Browser.downloadProgress", deadline, |params| {
            params["guid"] == guid.as_str() && params["state"] != "inProgress"
        });
        assert_eq!(done["state"], "completed", "the download did not complete");
        // `allowAndName` saves the file under its guid.
        let text = std::fs::read_to_string(directory.path().join(&guid))
            .expect("read the downloaded file");
        (name, text)
    }

    /// Gives the page a virtual WebAuthn authenticator (Chromium DevTools'
    /// `WebAuthn` domain): a platform authenticator that keeps discoverable
    /// credentials, verifies the user and needs no touch. It lives while the
    /// returned value does.
    ///
    /// # Panics
    /// If Chromium refuses a DevTools call.
    pub fn virtual_authenticator(&self) -> DevTools {
        let mut devtools = DevTools::connect(&self.browser);
        let attached = devtools.call(
            None,
            "Target.attachToTarget",
            serde_json::json!({ "targetId": self.page.target_id(), "flatten": true }),
        );
        let session = attached["sessionId"]
            .as_str()
            .expect("a DevTools session")
            .to_string();
        devtools.call(
            Some(&session),
            "WebAuthn.enable",
            serde_json::json!({ "enableUI": false }),
        );
        devtools.call(
            Some(&session),
            "WebAuthn.addVirtualAuthenticator",
            serde_json::json!({ "options": {
                "protocol": "ctap2",
                "transport": "internal",
                "hasResidentKey": true,
                "hasUserVerification": true,
                "isUserVerified": true,
                "automaticPresenceSimulation": true,
            }}),
        );
        devtools
    }

    /// Fails on any console record of type "error" since the page opened.
    /// Chromium's own "Failed to load resource" line for an HTTP error
    /// status is ignored: journeys provoke 401, 403 and 409 on purpose, and
    /// the console reports those through toasts, not script errors.
    pub fn assert_no_console_errors(&self) {
        let records = self
            .page
            .console_records(true, false)
            .expect("read the console");
        let errors: Vec<&str> = records
            .records
            .iter()
            .filter(|record| record.message_type == "error")
            .filter(|record| !record.text.starts_with("Failed to load resource"))
            .map(|record| record.text.as_str())
            .collect();
        assert!(errors.is_empty(), "console errors: {errors:?}");
    }
}

/// A DevTools connection of the harness's own, beside rustwright's: it holds
/// a virtual authenticator ([`Ui::virtual_authenticator`]), a download
/// directory's behaviour ([`Ui::download`]), network emulation
/// ([`Ui::go_offline`]) or response interception ([`PausedResponses`],
/// [`Tamper`]); dropping it closes the connection, which removes what it
/// set.
pub struct DevTools {
    socket: tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    next_id: u64,
    /// Sessions whose network emulation [`Ui::go_offline`] turned on;
    /// dropping the connection turns it off again on each of them.
    offline_sessions: Vec<String>,
    /// Events that arrived while [`DevTools::try_call`] waited for its
    /// answer, kept for [`DevTools::event`] (a paused response must not be
    /// lost while another is released).
    pending: std::collections::VecDeque<serde_json::Value>,
}

impl DevTools {
    /// Opens a connection to `browser`'s DevTools endpoint.
    ///
    /// # Panics
    /// If the connection fails.
    fn connect(browser: &Browser) -> Self {
        let (socket, _) = tungstenite::connect(browser.ws_endpoint()).expect("connect to DevTools");
        // A read timeout lets `event` give up at its deadline instead of
        // blocking forever on a quiet socket.
        if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_ref() {
            stream
                .set_read_timeout(Some(Duration::from_millis(200)))
                .expect("set a read timeout");
        }
        Self {
            socket,
            next_id: 0,
            offline_sessions: Vec::new(),
            pending: std::collections::VecDeque::new(),
        }
    }

    /// Turns network emulation on or off for one attached session.
    ///
    /// # Panics
    /// If Chromium refuses the call.
    fn set_offline(&mut self, session: &str, offline: bool) {
        self.call(
            Some(session),
            "Network.emulateNetworkConditions",
            serde_json::json!({
                "offline": offline,
                "latency": 0,
                "downloadThroughput": -1,
                "uploadThroughput": -1,
            }),
        );
    }

    /// The next JSON message, or `None` when the read timed out or the
    /// message is not JSON text.
    ///
    /// # Panics
    /// If the connection fails.
    fn read_json(&mut self) -> Option<serde_json::Value> {
        let message = match self.socket.read() {
            Ok(message) => message,
            // The read timeout: the caller looks at its clock and reads again.
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return None
            }
            Err(error) => panic!("read from DevTools: {error}"),
        };
        serde_json::from_str(message.to_text().ok()?).ok()
    }

    /// Waits for the next `method` event whose params satisfy `wanted`,
    /// skipping every other message (those kept while a call waited
    /// first).
    ///
    /// # Returns
    /// The event's params.
    ///
    /// # Panics
    /// When none arrives before `deadline`, or the connection fails.
    fn event(
        &mut self,
        method: &str,
        deadline: Instant,
        wanted: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let kept = self
            .pending
            .iter()
            .position(|event| event["method"] == method && wanted(&event["params"]));
        // `remove` hands back the owned event (an `Option`, like `list.pop(at)`).
        if let Some(event) = kept.and_then(|at| self.pending.remove(at)) {
            return event["params"].clone();
        }
        self.pending.clear();
        loop {
            assert!(Instant::now() < deadline, "no {method} event in time");
            let Some(event) = self.read_json() else {
                continue;
            };
            if event["method"] == method && wanted(&event["params"]) {
                return event["params"].clone();
            }
        }
    }

    /// Sends one DevTools command and waits for its answer, keeping the
    /// events that arrive meanwhile for [`DevTools::event`].
    ///
    /// # Panics
    /// If the connection fails or Chromium answers with an error.
    fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        self.try_call(session, method, params)
            .unwrap_or_else(|error| panic!("{method}: {error}"))
    }

    /// [`DevTools::call`], but Chromium's error answer is returned.
    ///
    /// # Returns
    /// The command's result, or the `error` object Chromium answered with.
    ///
    /// # Panics
    /// If the connection fails or no answer arrives in time.
    fn try_call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let id = self.send(session, method, params);
        let deadline = Instant::now() + WAIT;
        loop {
            assert!(Instant::now() < deadline, "no answer to {method} in time");
            let Some(reply) = self.read_json() else {
                continue;
            };
            if reply.get("method").is_some() {
                self.pending.push_back(reply);
                continue;
            }
            if reply["id"] == id {
                return match reply.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(reply["result"].clone()),
                };
            }
        }
    }

    /// Sends one DevTools command without waiting for its answer (for a
    /// loop that reads answers and events itself, as [`Tamper`]'s does).
    ///
    /// # Returns
    /// The command's id, which its answer carries.
    ///
    /// # Panics
    /// If the connection fails.
    fn send(&mut self, session: Option<&str>, method: &str, params: serde_json::Value) -> u64 {
        self.next_id += 1;
        let mut command =
            serde_json::json!({ "id": self.next_id, "method": method, "params": params });
        if let Some(session) = session {
            command["sessionId"] = serde_json::json!(session);
        }
        self.socket
            .send(tungstenite::Message::text(command.to_string()))
            .expect("send a DevTools command");
        self.next_id
    }
}

/// Responses paused in the reader's service worker (see
/// [`Ui::pause_worker_responses`]); dropping it closes its DevTools
/// connection, which ends the pausing and lets any still paused through.
pub struct PausedResponses {
    devtools: DevTools,
    session: String,
}

impl PausedResponses {
    /// Waits for the next paused response.
    ///
    /// # Returns
    /// Its DevTools request id, for [`PausedResponses::release`].
    ///
    /// # Panics
    /// When none is paused within [`WAIT`].
    pub fn next_paused(&mut self) -> String {
        let deadline = Instant::now() + WAIT;
        let params = self
            .devtools
            .event("Fetch.requestPaused", deadline, |_| true);
        params["requestId"]
            .as_str()
            .expect("a request id")
            .to_string()
    }

    /// Lets the paused response `request` through to the worker.
    ///
    /// # Panics
    /// If Chromium refuses (no such paused response).
    pub fn release(&mut self, request: &str) {
        self.devtools.call(
            Some(&self.session),
            "Fetch.continueResponse",
            serde_json::json!({ "requestId": request }),
        );
    }
}

/// Rewrites matching responses for every new service worker until dropped
/// (see [`Ui::tamper_new_workers`]).
pub struct Tamper {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    rewritten: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Tamper {
    /// Turns on auto-attach to new service workers (paused until their
    /// `Fetch` interception is on) on `devtools`, and starts the thread
    /// that answers its events until the guard is dropped.
    fn start(mut devtools: DevTools, pattern: &str, body: &str) -> Self {
        use base64::Engine as _;
        devtools.call(
            None,
            "Target.setAutoAttach",
            serde_json::json!({
                "autoAttach": true,
                "waitForDebuggerOnStart": true,
                "flatten": true,
                "filter": [{ "type": "service_worker" }],
            }),
        );
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rewritten = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // DevTools wants a fulfilled body in standard base64.
        let (pattern, body) = (
            pattern.to_string(),
            base64::engine::general_purpose::STANDARD.encode(body.as_bytes()),
        );
        // Clones of the `Arc`s move into the thread; the guard keeps its own.
        let (stopping, counting) = (
            std::sync::Arc::clone(&stop),
            std::sync::Arc::clone(&rewritten),
        );
        let thread = std::thread::spawn(move || {
            while !stopping.load(std::sync::atomic::Ordering::Relaxed) {
                let Some(event) = devtools.read_json() else {
                    continue;
                };
                match event["method"].as_str() {
                    Some("Target.attachedToTarget") => {
                        let Some(worker) = event["params"]["sessionId"].as_str() else {
                            continue;
                        };
                        let worker = worker.to_string();
                        devtools.send(
                            Some(&worker),
                            "Fetch.enable",
                            serde_json::json!({
                                "patterns": [{ "urlPattern": pattern, "requestStage": "Response" }],
                            }),
                        );
                        devtools.send(
                            Some(&worker),
                            "Runtime.runIfWaitingForDebugger",
                            serde_json::json!({}),
                        );
                    }
                    Some("Fetch.requestPaused") => {
                        devtools.send(
                            event["sessionId"].as_str(),
                            "Fetch.fulfillRequest",
                            serde_json::json!({
                                "requestId": event["params"]["requestId"],
                                "responseCode": 200,
                                "responseHeaders": [
                                    { "name": "Content-Type", "value": "text/css; charset=utf-8" },
                                ],
                                "body": body,
                            }),
                        );
                        counting.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        });
        Self {
            stop,
            rewritten,
            thread: Some(thread),
        }
    }

    /// How many responses it has rewritten so far.
    pub fn rewritten(&self) -> usize {
        self.rewritten.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Drop for Tamper {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // The thread sees `stop` within one read timeout (200 ms), then
        // drops its connection, which detaches from every worker.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for DevTools {
    /// Restores the network on every session [`Ui::go_offline`] cut off.
    /// A target that has gone away (a stopped worker) is skipped; nothing
    /// here panics, since this can run while a journey is already failing.
    fn drop(&mut self) {
        for session in std::mem::take(&mut self.offline_sessions) {
            let command = serde_json::json!({
                "id": self.next_id + 1,
                "method": "Network.emulateNetworkConditions",
                "sessionId": session,
                "params": {
                    "offline": false,
                    "latency": 0,
                    "downloadThroughput": -1,
                    "uploadThroughput": -1,
                },
            });
            self.next_id += 1;
            let id = self.next_id;
            if self
                .socket
                .send(tungstenite::Message::text(command.to_string()))
                .is_err()
            {
                return;
            }
            // Wait briefly for the answer so the command lands before close.
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                match self.socket.read() {
                    Ok(message) => {
                        let answered = message
                            .to_text()
                            .ok()
                            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                            .is_some_and(|reply| reply["id"] == id);
                        if answered {
                            break;
                        }
                    }
                    Err(tungstenite::Error::Io(error))
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => return,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #1101: the harness asks Chromium for port 0 exactly once, so
    /// rustwright reads the endpoint from `DevToolsActivePort` instead of
    /// handing Chromium a port it picked beforehand.
    #[test]
    fn the_launch_options_let_chromium_choose_its_debugging_port() {
        let home = BrowserHome::create("/bin/true").expect("create a browser folder");

        let options = launch_options(&home);

        let port_args: Vec<&String> = options
            .args
            .iter()
            .filter(|arg| arg.starts_with("--remote-debugging-port"))
            .collect();
        assert_eq!(port_args, ["--remote-debugging-port=0"]);
        assert!(
            options.args.iter().any(|arg| arg == LOCALHOST_IS_IPV4),
            "the IPv4 rule must stay"
        );
        assert_eq!(
            options.user_data_dir.as_deref(),
            Some(home.profile().to_string_lossy().as_ref()),
            "DevToolsActivePort is read from the harness's profile folder"
        );
    }
}
