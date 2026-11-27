// The reader's boot and router (reader design). It registers the worker
// and waits for it to control the page, checks the session, then renders
// the screen the hash names. (Which account the cached copies belong to is
// the worker's decision, sw.js `admit`.) Every ReadError a screen throws
// comes back here, to a refusals.js handler, the one place that decides
// what a refusal does.
import { $, h, clear } from '/ui/js/core/dom.js';
import { ago } from '/ui/js/core/format.js';
import { read, readAll, signOut, ReadError, KIND } from './api.js';
import { refusals } from './refusals.js';
import { wipe, evictCatalog, evictDocument } from './cache.js';
import { parse, hrefs, parentOf } from './route.js';
import { message } from './parts.js';
import * as home from './screens/home.js';
import * as catalogs from './screens/catalogs.js';
import * as tree from './screens/tree.js';
import * as search from './screens/search.js';
import * as concept from './screens/concept.js';
// Not `document`: that name would hide the browser's global in this module.
import * as documentScreen from './screens/document.js';
import * as me from './screens/me.js';

const SCREENS = { home, catalogs, tree, search, concept, document: documentScreen, me };
const NEEDS_CATALOG = new Set(['home']);
const CATALOG_KEY = 'okf.read.catalog';

// `known`: the catalogs the account reaches, by name (GET /catalogs: label,
// page count), once read; `titles`: each catalog's folder titles (folder
// path to title), as its tree was last read, for breadcrumbs.
const state = { session: null, catalog: remembered(), known: null, titles: new Map() };
// The catalog list's read, shared by every screen of this page load.
let catalogRead = null;
// Bumped on every navigation, so a late answer never draws over a newer screen.
let generation = 0;
// True while the page is leaving this account (a sign-out under way, or a
// 401 on its way to sign-in): no catalog is remembered meanwhile, so one
// chosen during the wipe or the DELETE never outlives the account.
let leaving = false;
// True once a 401 has shown the signed-out screen: no screen is drawn after.
let signedOut = false;

function remembered() {
    try { return localStorage.getItem(CATALOG_KEY); } catch { return null; }
}

// Stores `catalog` as the remembered one (null deletes it).
function remember(catalog) {
    try {
        if (catalog) localStorage.setItem(CATALOG_KEY, catalog);
        else localStorage.removeItem(CATALOG_KEY);
    } catch { /* storage blocked: the choice lasts for this visit only */ }
}

// Makes `catalog` the current one (null forgets it). While the page is
// leaving the account it is current for this page only, never remembered.
function choose(catalog) {
    state.catalog = catalog;
    remember(leaving ? null : catalog);
    tabs();
}

function signInUrl() {
    return `/signin?return_to=${encodeURIComponent(`/read/${location.hash}`)}`;
}

// Deletes everything this device keeps of the account: every offline copy
// and the remembered catalog, so the next person to sign in here never sees
// the last one's catalog name. Sign-out, "Delete offline copies" and a 401
// all go through here.
async function forget() {
    choose(null);
    await wipe();
}

// Deletes the offline copies and the remembered catalog once more on the
// way out of the account: a read the worker admitted while the sign-out's
// DELETE was pending (the session still held then) may have cached a copy,
// and owned it, after the first wipe. Once the server has ended the session
// no later read is admitted, and this wipe's mark drops any answer still on
// its way.
async function wipeOnExit() {
    await wipe();
    remember(null);
}

// Leaves the account for the sign-in page at `url`.
async function leaveFor(url) {
    await wipeOnExit();
    location.assign(url);
}

// The session is gone (a 401): deletes what this device keeps of the
// account, then shows the reader's own signed-out screen instead of
// navigating away, so the page keeps its manifest and worker and stays
// installable. Its button goes to the sign-in page.
async function signIn() {
    leaving = true;
    signedOut = true;
    generation += 1;
    await forget();
    await wipeOnExit();
    showSignedOut();
}

// Draws the signed-out screen: no header or tabs, a notice and a "Sign in"
// link that returns to the reader's current address.
function showSignedOut() {
    document.body.classList.add('signed-out');
    $('#offline-banner').hidden = true;
    clear($('#reader')).append(h('section', { class: 'screen', id: 'screen-signed-out' },
        message('You are signed out.',
            h('a', { href: signInUrl(), id: 'signed-out-signin', role: 'button' }, 'Sign in'),
            'signed-out')));
    $('#reader').hidden = false;
}

// Signs out (the account screen's button): deletes the offline copies and
// the remembered catalog, then ends the session and leaves for sign-in.
// Returns false, keeping the session, when the server could not be reached
// or refused; catalogs are remembered again from then on.
async function endSession(csrf) {
    leaving = true;
    await forget();
    try {
        if (await signOut(csrf)) {
            await leaveFor(`/signin?return_to=${encodeURIComponent('/read/')}`);
            return true;
        }
    } catch (error) {
        if (!(error instanceof ReadError)) throw error;
    }
    leaving = false;
    return false;
}

// Shows the offline banner when `result` came from the worker's cache.
function freshness(result) {
    if (!result?.offline) return;
    const banner = $('#offline-banner');
    banner.textContent = `Offline. Synced ${ago(result.cachedAt)}.`;
    banner.hidden = false;
}

// The catalogs the account may read now, by name, read once per page load (the
// worker keeps a copy for offline). A refused read gives an empty map and
// is tried again next time: the header then shows the catalog's name, and
// each screen's own reads decide what the refusal means.
function catalogList() {
    catalogRead ??= readAll('/catalogs?readable=true', 'catalogs').then(result => {
        state.known = new Map(result.data.map(found => [found.name, found]));
        relabel();
        return state.known;
    }, error => {
        catalogRead = null;
        if (error instanceof ReadError) return new Map();
        throw error;
    });
    return catalogRead;
}

// Shows the catalogs' labels wherever the page names a catalog: the header,
// and every element a screen marked data-catalog-label="<name>" (the
// breadcrumbs' root) while the list was still being read. The one place
// that updates them; screens drawn later read the labels from state.known
// (ctx.label()). Never blocks a screen.
function relabel() {
    header();
    for (const element of document.querySelectorAll('[data-catalog-label]')) {
        const label = state.known?.get(element.dataset.catalogLabel)?.label;
        if (label) element.textContent = label;
    }
}

// The header: the current catalog's mark and label, and where its search
// icon goes.
function header() {
    const catalog = state.catalog;
    const label = (catalog && state.known?.get(catalog)?.label) || catalog || 'Reader';
    $('#bar-label').textContent = label;
    $('#bar-mark').textContent = ([...label.trim()][0] ?? 'R').toUpperCase();
    $('#bar-search').setAttribute('href', catalog ? hrefs.search(catalog) : hrefs.catalogs());
}

function tabs() {
    const current = parse(location.hash).screen;
    const catalog = state.catalog;
    $('#tab-browse').setAttribute('href', catalog ? hrefs.tree(catalog) : hrefs.catalogs());
    $('#tab-search').setAttribute('href', catalog ? hrefs.search(catalog) : hrefs.catalogs());
    header();
    const active = { home: 'tab-home', tree: 'tab-browse', concept: 'tab-browse', document: 'tab-browse', search: 'tab-search', me: 'tab-me' }[current];
    for (const tab of document.querySelectorAll('.tabs a')) {
        if (tab.id === active) tab.setAttribute('aria-current', 'page');
        else tab.removeAttribute('aria-current');
    }
}

// Draws a refusal's notice on `view` (the screen at `route`); refusals.js
// decides when, and which refusals draw something else.
function paint(view, route, error) {
    switch (error.kind) {
        case KIND.CONCEPT_GONE:
            clear(view).append(message('This concept no longer exists.',
                h('a', { href: hrefs.tree(route.catalog, parentOf(route.path)), id: 'gone-folder' }, 'Open its folder'), 'concept-gone'));
            return;
        case KIND.LOCKED:
            clear(view).append(message(error.message,
                error.body?.error === 'UnlockRequired' ? h('a', { href: '/ui/', id: 'unlock-link' }, 'Unlock in the console') : null, 'locked'));
            return;
        case KIND.UNAVAILABLE:
            clear(view).append(message('Not available offline.',
                h('a', { href: hrefs.home(), id: 'offline-list-link' }, 'See what is available offline'), 'unavailable'));
            return;
        default:
            clear(view).append(message(error.message, null, 'failed'));
    }
}

async function show() {
    if (signedOut) return;
    const parsed = parse(location.hash);
    const mine = ++generation;
    if ((parsed.catalog ?? null) !== null && parsed.catalog !== state.catalog) choose(parsed.catalog);
    // The catalog this screen reads, fixed now: a refusal that answers after
    // the user switched catalogs must act on the one that was asked about.
    const route = { ...parsed, catalog: parsed.catalog ?? state.catalog };
    tabs();
    $('#offline-banner').hidden = true;
    if (NEEDS_CATALOG.has(route.screen) && !route.catalog) {
        location.replace(hrefs.catalogs());
        return;
    }
    const view = h('section', { class: 'screen', id: `screen-${route.screen}` });
    clear($('#reader')).append(view);
    // Every screen change starts at the top (a section route scrolls to its
    // heading once drawn).
    window.scrollTo(0, 0);
    const live = () => mine === generation;
    const refusal = refusals(route, live, {
        signIn, evictCatalog, evictDocument, choose,
        current: () => state.catalog,
        go: hash => { location.hash = hash; },
        paint: error => paint(view, route, error),
    });
    const ctx = {
        state, route, choose, forget, signOut: endSession, freshness, live,
        checkSession, SESSION,
        fail: refusal.fail,
        settle: refusal.settle,
        drawWorst: refusal.drawWorst,
        // The current catalog's entry of GET /catalogs ({ label, concepts, ... }), or null.
        summary: () => catalogList().then(found => found.get(route.catalog) ?? null),
        // Its label now: its name until the list has been read.
        label: () => state.known?.get(route.catalog)?.label ?? route.catalog,
        // Its folder titles from the last tree read, or null.
        titles: () => state.titles.get(route.catalog) ?? null,
        rememberTitles: titles => { state.titles.set(route.catalog, titles); },
    };
    try {
        await SCREENS[route.screen].render(view, ctx);
        // A screen drawn after the list was read may still name its catalog
        // by name (a crumb without `root`): relabel it now.
        if (state.known && live()) relabel();
    } catch (error) {
        await refusal.caught(error);
    }
}

// A warning, not an error: the reader still works online.
function registrationFailed(error) {
    console.warn('the reader\'s worker could not be registered:', error?.message);
}

// How long a first visit waits for the new worker to take control.
const WORKER_WAIT_MS = 5000;

// The reader decides where each screen starts (at the top), not the browser.
if ('scrollRestoration' in history) history.scrollRestoration = 'manual';
// The header's hairline shows once the page is scrolled.
window.addEventListener('scroll', () => {
    $('#top').classList.toggle('scrolled', window.scrollY > 4);
}, { passive: true });

// Registers the worker and resolves once it controls this page, so the
// first screen's reads already go through it and are cached (on a first
// visit, a direct concept link included). It resolves anyway, without a
// worker, when registration fails or takes too long: the reader then works
// online and only offline reading is lost.
function registerWorker() {
    if (!('serviceWorker' in navigator)) return Promise.resolve();
    const workers = navigator.serviceWorker;
    if (workers.controller) {
        // Already controlled: this checks for a newer build in the
        // background, and the worker it has keeps serving if that fails.
        workers.register('/read/sw.js', { scope: '/read/' }).catch(registrationFailed);
        return Promise.resolve();
    }
    return new Promise(resolve => {
        const timer = setTimeout(resolve, WORKER_WAIT_MS);
        const done = () => { clearTimeout(timer); resolve(); };
        // sw.js calls skipWaiting and clients.claim, so a first install
        // takes this page over and fires controllerchange.
        workers.addEventListener('controllerchange', done, { once: true });
        workers.register('/read/sw.js', { scope: '/read/' }).catch(error => {
            registrationFailed(error);
            done();
        });
    });
}

// Reads GET /session and says what it meant: SESSION.ANSWERED (state.session
// is now the answer), SESSION.SIGNED_OUT (the signed-out screen is drawn), or
// SESSION.OFFLINE (the server could not be reached, or failed; state.session
// is left as it was, so other screens behave as before). Boot and the Account
// screen both go through here, so they classify a failed read the same way.
const SESSION = Object.freeze({ ANSWERED: 'answered', SIGNED_OUT: 'signed-out', OFFLINE: 'offline' });

async function checkSession() {
    try {
        const { data } = await read('/session');
        state.session = data;
        return SESSION.ANSWERED;
    } catch (error) {
        if (!(error instanceof ReadError)) throw error;
        if (error.kind === KIND.SIGNED_OUT) {
            await signIn();
            return SESSION.SIGNED_OUT;
        }
        return SESSION.OFFLINE;
    }
}

async function boot() {
    await registerWorker();
    // Offline (or the server failing): read what is cached; the banner says
    // so on the first cached answer.
    if (await checkSession() === SESSION.SIGNED_OUT) return;
    // Not awaited: the first screen never waits for the labels.
    catalogList();
    $('#reader').hidden = false;
    window.addEventListener('hashchange', show);
    await show();
}

boot();
