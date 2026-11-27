# Mobile reader

A read-only web app at `/read/` for reading a catalog on a phone, offline
included. It signs in with the same account as the [web
console](../operations.md#web-console) (a console session cookie) and needs no
API key. It changes nothing: every request it makes is a `GET` under
`/api/v1`, except the sign-out `DELETE /api/v1/session`.

The examples use `https://reader.example.com` for your server's public
address.

## Install

- **iPhone and iPad:** open `https://reader.example.com/read/` in Safari, tap
  Share, then Add to Home Screen. Until it is installed, Home shows a hint
  saying so; "Got it" dismisses it for good. The installed app keeps its own
  cookies, so sign in once inside it (a passkey makes that quick).
- **Android:** open the same address in Chrome and accept the install prompt,
  or install it from Chrome's menu.

Visiting `/read/` while signed out stays on the reader: it shows its own
"You are signed out" screen with a **Sign in** button. The page keeps its
manifest and worker there, so Chrome offers to install it before anyone has
signed in. The button goes to the sign-in page and comes back to the same
screen afterwards (`return_to`, which accepts same-origin `/read/` paths).

### Name

The installed app is named after the server's `product_name` setting
([Settings](../operations.md#settings); the config file's `[auth] product_name`
until it is set there): the manifest's `name` is "Acme Notes Reader", its
`short_name` (the home-screen label) is "Acme Notes", and the tab title and the
iOS home-screen title follow the same rule. `/read/` and
`/read/manifest.webmanifest` are rendered per request from the current setting
(escaped for HTML and JSON), so a rename needs no restart.

## Screens and routes

The reader is one page; its screen is in the URL hash, every segment
percent-encoded. Four tabs sit at the bottom: Home, Browse, Search and
Account.

| Route | Screen |
|---|---|
| `#/` | Home: the install hint (iPhone and iPad browsers only), a search box that counts the catalog's pages ("Search 1,284 pages"), "Jump back in" (the three concepts and documents this device viewed last, from its cached copies, each with its folder and "Synced 2 h ago"), and the Activity timeline: filter chips (All, Created, Edited, Deleted, By me), day headings, and changes grouped by actor and batch (the same actor and message, less than 2 minutes apart), with "+N more" and "Show older". When the server cannot be reached or the worker answered from its copy, every offline copy is listed under "Available offline" instead of "Jump back in", and the timeline, when the worker holds recent changes, is drawn from that copy |
| `#/c` | The catalog picker: each catalog with its mark, label, description and page count. With exactly one reachable catalog and none chosen, it chooses that one and opens Home. `#/c?gone=<catalog>` says you can no longer read that catalog and its offline copies are deleted |
| `#/t/<catalog>[/<folder>...]` | One folder: breadcrumbs (the catalog's label, then folder titles), its title and description, an About row that opens its `index.md`, its subfolders (description and page count), its concepts (description) under "Pages", and a Log row for its `log.md`. Logs and machinery files are not listed among the pages |
| `#/s/<catalog>[?q=<query>]` | Search results in that catalog: title, folder and a plain-text snippet. "Show more" adds 50 more, up to 200; "50+ results" means there may be more, and at 200 the screen says to refine the search |
| `#/v/<catalog>/<path>.md[?s=<slug>]` | A concept: breadcrumbs, title, a meta row (its `type` and up to three `tags` as pills, the reading time, and "Edited 1 h ago by Name"), its description as a lede, "On this page" (a collapsible list of every heading but the title), the body in the serif reading type, and "Linked from" with one row per linking document. The reader always reads the whole body. `?s=<slug>` scrolls to a heading |
| `#/d/<catalog>/<path>` | A folder's `index.md` (shown as About), a `log.md` (Log) or `AGENTS.md`, read as a document |
| `#/me` | Account |

Details worth knowing:

- A concept's path is every segment after the catalog, so a folder named
  `x.md` is still a folder; its heading travels as `?s=`, never as a segment.
- Every link the reader draws (recent changes, backlinks, search hits, links
  inside a body, the offline list) opens the right screen: a folder's
  `index.md` opens that folder, a `log.md` or `AGENTS.md` the document view,
  anything else the concept view. Links to the console's browse routes in a
  body are rewritten to reader routes; web links open outside the app.
- A concept is always read whole (`?full=true`): there is no "Show the whole
  concept" button. The page title is not repeated in the body, and the
  outline lists every other heading whatever its level. A concept whose
  first level-1 heading is not its title keeps that heading.
- "Linked from" lists each linking document once: "N links" when it links
  more than once, "N entries" for a `log.md`, which shows as Log.
- The edit line ("Edited 1 h ago by Name") comes from the history route and
  fills in when it answers; the concept never waits for it. Offline, or when
  the history is refused, the line is left out. The reading time is
  estimated from the length of the text (1,200 characters a minute).
- "Synced 2 h ago" on Home is the time of the last network read of that
  copy. Reading a copy offline does not change it, so it says when the
  copy was synced, not when it was read. The order is by viewing, not by
  that time: opening a copy offline moves it to the front without changing
  its "Synced" time.
- Search needs a connection. Offline it says so and links to Home's offline
  list.
- Account shows who is signed in (with an avatar) and when the session
  ends, an Appearance choice (System, Light, Dark; kept on the device and
  applied before the first paint, and it also sets the colour of the system
  bars), a **Sign out** button and a **Delete offline copies** button.
  Account asks the server for the session each time it opens; when the
  server cannot be reached (also after a start that was online), it shows
  the Appearance choice and the latter button, because the account details
  and sign-out need the server. The choice survives signing
  out.
- The header shows the current catalog's label (its name until the catalog
  list has been read) after a mark with its first letter, opens the picker,
  and has a search icon; the bottom tabs have icons. Every screen change
  starts at the top of the page.
- The chosen catalog is remembered in the browser's `localStorage` (the
  install-hint dismissal and the Appearance choice too). Where storage is blocked, the choice lasts for
  that visit.

## Offline

The reader's own files (the shell) are cached when it is installed and served
cache-first. Catalog reads go through the service worker, which is scoped to
`/read/` and never controls a console page.

- **Network-first.** A read is answered by the server when it can be, and that
  answer is remembered when it is a 2xx. Only when the network fails is the
  remembered copy served, and the page then shows "Offline. Synced ...".
  So a revoked session never sees cached content while online.
- **What is kept.** Concepts and documents (`log.md`, `AGENTS.md`, folder
  indexes) are kept up to 200 cached copies (a concept held in both short and whole
  form counts twice), and the least recently viewed is dropped first:
  reading a copy offline counts as viewing it. The catalog list, each tree,
  recent changes and each concept's links are kept too, uncapped. Search,
  the session and the account screen are never cached.
- **Short and full.** The reader asks for the whole body, so it keeps whole
  copies. Offline, the worker still lets a whole-body copy answer a plain
  request, and a plain copy answer a whole-body request when it holds the
  whole body (it was not shortened), so copies kept by an earlier version of
  the reader stay readable.
- **Home offline.** "Available offline" lists the concepts and documents of the
  current catalog that this device holds, most recently viewed first, each
  with when it was synced. Online, "Jump back in" shows the first three of the same list. A
  concept you never opened says "Not available offline." and links there.
  When the worker answers Home from its copy of recent changes, the
  Activity timeline is drawn below the list as it was last read; "Show
  older" then says it needs a connection and keeps what is drawn.
- **One account.** Every answer an `/api/v1` route gives a signed-in caller
  carries `X-OKF-Account`, an opaque tag of the account it was made for ([operations](../operations.md#web-console)).
  The worker keeps one recorded owner for all copies. The first successful
  answer for a different account, cached or not (boot's session check and
  a search count too; the worker reads their tag and never keeps their
  bodies), deletes the old copies and takes over, and copies are
  served offline only to their owner, so a second account signing in on the
  same device, even in another tab, never reads the first one's. An answer
  with no tag is not cached. With no recorded owner, nothing is served offline.

### When copies are deleted

| What happens | What the reader does |
|---|---|
| You sign out | Deletes every offline copy and the remembered catalog first, then ends the session, and deletes them a second time once the server has confirmed it (a read admitted while the request was pending cannot leave a copy behind). If the server cannot be reached, the session is kept, the copies are already gone, and the screen says so. |
| **Delete offline copies** | Deletes every copy and the remembered catalog. It works with no connection. |
| A `401` anywhere (session expired, revoked, account removed), at boot or later | The same wipe, then the signed-out screen with its **Sign in** button (the page does not navigate away). It acts at once, even when the screen that asked has been left or a sibling read is still pending. |
| You lose your grant to a catalog (`403 Forbidden`, or `404` for a catalog not selected or unknown) | Deletes that catalog's copies only and forgets it if it is the remembered one. If the screen that asked is still showing, it opens the picker with a notice. |
| A concept or document is deleted (`404 NotFound`) | Deletes that document's copies (short and full) and says "This concept no longer exists." with a link to its folder. |
| The catalog needs an unlock, or its team is suspended (`403 UnlockRequired`, `Suspended`) | Keeps the copies. The screen says why, and for an unlock links to the console. |
| You sign in as another account (a tag the worker has not seen) | The worker deletes the old account's copies, including from another tab. |
| An answer is still on its way when any deletion starts | The page tells the worker first, and waits for its reply. The worker never stores an answer to a request that began at or before that mark, so a slow success cannot put back what a 401 or a lost grant removed. |

Booting without a connection keeps the copies and shows what is cached; the
account screen then says it needs a connection.

## Storage

- Where Cache Storage is missing or refuses (denied by the browser's settings,
  a full disk, a private mode), the reader works online and keeps nothing. The
  worker's writes are best effort and the page always gets the network's
  answer. Deleting needs no free space, so the buttons still work.
- Without a service worker (registration failed, an insecure context) the
  reader works online only. A first visit waits up to five seconds for the
  worker to take control, so the first concept opened is already cached.

## Updates and the verified install

The worker's version is a hash of every file it precaches: the reader's own
and the console's Pico CSS, `dom.js` and `format.js`, and the product name
(which `index.html` and the manifest carry). Any change to them changes
`/read/sw.js` (served with `Cache-Control: no-cache`), and installed apps
update on their next launch; a rename made in Settings reaches them the same
way. The listing is built per request, so it always names the current product.

`GET /read/shell.json` lists the version and each file with the SHA-256 of
the bytes this build serves:

    {"version": "...", "files": [{"url": "/read/", "sha256": "..."}, ...]}

The worker installs only when the listing carries its own version and every
file it fetches hashes as listed. A deploy between its requests, or replicas
on two builds, fails the install instead of caching a mix; the browser keeps
the worker it has and tries again on a later visit. On activation, a new
worker deletes the shell caches of other versions.

## Known limits

- **iOS may evict storage.** An installed app's storage can be cleared after
  weeks without use. The offline copies are best effort.
- **Separate cookie jar on iOS.** The installed app has its own cookies, so you
  sign in once inside it.
- **A lost, unlocked phone** shows cached concepts until it next reaches the
  server (or until you delete them from the account screen on it).
- **A retired worker cannot be marked.** A worker the browser has already
  made redundant, but lets finish a request, cannot be reached by any message.
  The page deletes after one second without a reply, so revocation never
  blocks on it, but an answer that worker was already fetching is not stopped.
- **Cache Storage can refuse every delete.** If it refuses to delete both the
  old copies and the old owner record during a takeover, the worker keeps
  nothing new and serves nothing. A later answer retries the takeover, and
  once one succeeds it deletes the old copies and records the new owner, so
  the old account's copies are gone. Only a worker restart while the old
  record and copies are still on disk finds them again, consistent, and serves
  them offline, until a takeover succeeds. That is the same exposure as signing
  in as someone else and going offline before any answer.

## For maintainers

- **Files.** `src/http/read/` is embedded by `build.rs` as `READ_ASSETS` and
  served by `src/http/read.rs` with the console's security headers.
  `index.html`, `manifest.webmanifest`, `icons/` (the install icons),
  `icons.svg` (the sprite the screens draw from), `fonts/` (Inter and
  Literata, Latin subsets, with their SIL Open Font License files),
  `read.css`, `sw.js`, and `js/`: `main.js` (boot, router, header, sign-out),
  `refusals.js` (what a refused read does, without the DOM), `api.js`,
  `cache.js` (the page's side of the caches), `names.js` (the cache names and
  the 200 cap, shared by page and worker), `route.js`, `listing.js`,
  `parts.js` (the parts more than one screen draws), `icons.js`, `theme.js`
  (a classic script that applies the Appearance choice before the first
  paint), the display modules `text.js`, `timeline.js`, `outline.js`,
  `backlinks.js` and `avatar.js` (pure, no DOM), and `screens/*.js`. The
  reader imports only `dom.js` and `format.js` from the console.
- **Styling.** `read.css` sits on Pico CSS and is independent of the console's
  `styles.css`; only Pico is shared. It holds the design tokens (light, dark
  from the system, and an explicit `data-theme` from Account), maps Pico's
  variables onto them, and has a section for the shared parts and one per
  screen. It has no inline styles (the content security policy forbids them):
  every variant is a class. Both stylesheets style the HTML `src/render.rs`
  writes into `body_html`, which is the one contract between them. The fonts
  are served from the server's own origin (`font-src 'self'`). They are built
  by `scripts/subset-reader-fonts.sh` from the upstream release files; check
  the outputs with `sha256sum -c scripts/reader-fonts.sha256`.
- **Tests.**
  - `just test-worker` (or `tools/agentic/gate.sh worker`, which adds a
    5 minute limit) runs `tests/read_worker/` under Node's built-in test
    runner with no npm packages: the worker in a `node:vm` context against an
    in-memory Cache Storage, and the DOM-free page modules (`api`, `cache`,
    `refusals`, `route`, `catalogs`, `tree` and `theme`, and the display
    modules' `text`, `timeline`, `outline`, `backlinks` and `avatar`). It needs Node
    18.19+ (or 20.6+) and skips with a message without `node` or with an older
    one. `just test` and `tools/agentic/gate.sh test` run it first.
  - The Rust unit tests in `src/http/read.rs` cover the routes, the headers,
    the theme colours and `shell.json`.
  - The browser journeys are in `tests/ui_e2e/read.rs` (`just test-ui`): sign-in
    and sign-out, the screens, offline reading, the cap, shell upgrades, full
    or denied storage, revoked sessions and grants, unlock refusals, late
    answers and two accounts, and the polish: the header label, scroll reset,
    the timeline, Show older and Show more, Jump back in, the concept's meta
    row and outline, and the theme choice. Five journeys pin the style
    overrides that Pico's rules would otherwise undo (the plain concept body,
    the outline's gaps and focus colour, the breadcrumbs, the Home search
    box, and the meta separator that trails a fact and never leads one), by
    reading computed styles.
  - `serve_a_sample_catalog_for_the_visual_check` in the same file is
    ignored: it serves a sample catalog of placeholder content for the
    visual check below, for `READER_VISUAL_MINUTES` (30 by default). The
    actors it shows are `tester`, `sam`, `alex` and `owner`, never a real
    login.
- **Manual checks before a release** (a phone and a deployed build): install
  on an iPhone (Safari, Share, Add to Home Screen) and on Android (Chrome's
  install prompt); the app opens standalone; content clears the notch and home
  indicator; light and dark follow the system; open three concepts, switch on
  airplane mode, relaunch and read them with the offline banner. Also run the
  visual check on a phone, in light and dark: serve the sample catalog and
  compare each screen with the design (Task 10 of
  [`docs/superpowers/plans/2026-11-21-reader-polish.md`](../superpowers/plans/2026-11-21-reader-polish.md)).
  The system navigation bar's colour (the page's `theme-color` metas) can
  only be judged in the installed app.
- The designs are in
  [`docs/superpowers/specs/2026-11-20-mobile-reader-pwa-design.md`](../superpowers/specs/2026-11-20-mobile-reader-pwa-design.md)
  and
  [`docs/superpowers/specs/2026-11-21-reader-polish-design.md`](../superpowers/specs/2026-11-21-reader-polish-design.md).
