// The signed-in identity in the shell: who, which catalogs, which groups,
// and the unassigned count on the Users link; and the operator's brand.
// Pages that change keys or catalogs call refreshIdentity() afterwards, and
// those that assign a user or owner call refreshUnassigned().
import { $, h } from './dom.js';
import { store } from './store.js';
import { api } from './api.js';
import { catalogLabel, setLabels } from './labels.js';
import { openFeedback } from '../components/feedback.js';

export async function refreshIdentity() {
    // Signed in with an account, the session (admin flag, CSRF token) is
    // read again beside whoami, whose `levels` give a member's level on
    // each catalog: an admin may have changed either.
    // The labels are read beside them: a failed read leaves slugs showing
    // rather than failing the whole identity.
    const [session, me, labels] = await Promise.all([
        store.session ? api.get('/session') : null,
        // Every page: the switcher offers every catalog this identity may
        // read now (`readable`: an admin's locked catalogs are left out; the
        // Catalogs page and the Unlock drawer still list them), and the
        // labels table names every catalog the key reaches.
        api.pages('/whoami?readable=true', 'catalogs'),
        api.pages('/labels', 'catalogs', { quiet: true }).catch(() => null),
    ]);
    if (session) {
        store.session = session;
        store.csrf = session.csrf;
    }
    store.whoami = me;
    setLabels(labels);
    // A console session is its user; a key shows its user and its name.
    const who = me.user_label ?? me.user;
    $('#whoami').textContent = me.kind === 'session'
        ? `${who} - ${me.access}`
        : `${me.user ? `${who} / ` : ''}${me.name ?? me.key_id} - ${me.access}`;
    if (!me.catalogs.includes(store.catalog)) {
        store.catalog = me.catalogs.includes(me.default_catalog) ? me.default_catalog : (me.catalogs[0] ?? null);
    }
    // After the catalog is settled: a member's level depends on it.
    document.querySelectorAll('[data-need]').forEach(el => { el.hidden = !shows(el.dataset.need); });
    const select = $('#catalog-select');
    select.replaceChildren(...me.catalogs.map(name => h('option', { value: name }, catalogLabel(name))));
    select.value = store.catalog ?? '';
    // The feedback form needs an account's session: drawn again now the
    // credential is known.
    drawHelp();
}

// Whether an element marked data-need="<need>" shows: "session" for the
// account pages (signed in with an account, not a key), "audit", "records",
// "account" (below), else an access level.
// "audit" for the Audit link: an admin that reaches every catalog, or anyone
// signed in with an account (every user owns at least its personal team, and
// the page lists the teams it owns). "records" for the Unlocks link: those,
// and any admin credential (a scoped admin reads the unlocks of the
// catalogs it administers). "account" for the Your account group, which
// holds the Catalogs link: an admin credential (key or account) or anyone
// signed in with an account (a member's Catalogs drawer holds the
// maintenance switch of the catalogs their teams own). An admin by key sees
// that link alone, as the Administer group had it.
function shows(need) {
    if (need === 'session') return !!store.session;
    if (need === 'audit') return canAdministerPeople() || !!store.session;
    if (need === 'records') return shows('audit') || store.whoami?.access === 'admin';
    if (need === 'account') return !!store.session || store.can('admin');
    return store.can(need);
}

// The account pages (My keys, My grants, Account) are for a person signed
// in with their account. Signed in by key, the page says so in a muted
// paragraph with this `id`. Returns whether the page may render.
export function requireSession(main, id) {
    if (store.session) return true;
    main.append(h('p', { class: 'muted', id },
        'This page is for people signed in with their account. Sign out, then sign in with your account to use it.'));
    return false;
}

// GET /api/v1/me, fresh on every load of an account page: everything it
// shows comes from this answer, never from the sign-in snapshot, since an
// admin may have changed it since. It names the ceiling's highest level
// only (`ceiling_access`); the ceiling itself is paged (GET /me/ceiling,
// components/ceiling.js). Rejects as api.get does (already toasted).
export async function freshAccount() {
    return api.get('/me');
}

// How the console leaves a session it ended itself: main.js installs it
// (the shell's Sign out uses the same one), and the Account page calls
// signedOut() after signing this browser's own session out.
let leave = () => {};
export function setSignedOutHandler(handler) { leave = handler; }
export function signedOut(message) { leave(message); }

// After a key's user changed: when it is the signed-in key, its user is
// shell state (#whoami, the Users page's "you" marker), so reload whoami.
// Every path that assigns a key calls this.
export async function keyUserChanged(keyId) {
    if (keyId === store.whoami?.key_id) await refreshIdentity().catch(() => {});
}

// Whether this key may administer users, teams and ownership: an admin key
// that reaches every catalog (plan Decision 3). Others see those controls
// disabled.
export function canAdministerPeople() {
    return store.can('admin') && !!store.whoami?.all_catalogs;
}

let unassignedTicket = 0;

// The count of keys, grants, codes and catalogs with no user or owner, on
// the Users link, for keys that may assign them; hidden at zero and for
// everyone else. Only the newest call writes, as for the findings badge.
export async function refreshUnassigned() {
    const badge = document.getElementById('unassigned-badge');
    if (!badge) return;
    const mine = ++unassignedTicket;
    if (!canAdministerPeople()) { badge.hidden = true; return; }
    let rows = null;
    try { rows = await api.get('/unassigned', { quiet: true }); } catch { /* hidden below */ }
    if (mine !== unassignedTicket) return;
    const total = rows ? rows.keys.length + rows.codes.length + rows.catalogs.length : 0;
    badge.textContent = String(total);
    badge.hidden = total === 0;
}

let brandTicket = 0;
// The newest /ui/brand.json answer, for drawHelp().
let brand = null;

// The sidebar's help entries, from the last brand.json answer: the
// "Documentation" link (Settings: docs_url), shown to everyone, the
// "Billing" link (Settings: billing_url) under "Your account", shown only
// to someone signed in with an account (never by key alone), and one
// feedback entry. The in-console form (#feedback-open, brand.json's
// `feedback_form`, there with a `[feedback]` table) needs an account's
// session, so for someone signed in that way it wins over the external
// "Send feedback" link (Settings: feedback_url); otherwise the link shows
// when set. With neither, no feedback entry shows. The server validated
// the URLs (https only); they are set as properties, never built into HTML.
function drawHelp() {
    const docs = document.getElementById('docs-link');
    const link = document.getElementById('feedback-link');
    const open = document.getElementById('feedback-open');
    const url = value => (typeof value === 'string' && value ? value : null);
    const setLink = (el, target) => {
        if (target) el.href = target; else el.removeAttribute('href');
        el.hidden = !target;
    };
    setLink(docs, url(brand?.docs_url));
    setLink(document.getElementById('billing-link'), store.session ? url(brand?.billing_url) : null);
    const form = store.session && brand?.feedback_form ? brand.feedback_form : null;
    open.hidden = !form;
    const feedbackUrl = form ? null : url(brand?.feedback_url);
    setLink(link, feedbackUrl);
    // For the browser tests: the entries above now follow a brand.json
    // answer (until then every one is hidden, as index.html draws them).
    if (brand) document.getElementById('app').dataset.brandLoaded = 'true';
}

// The feedback button opens the form drawn from the brand it was shown for.
document.getElementById('feedback-open')?.addEventListener('click', () => {
    if (brand?.feedback_form) openFeedback(brand.feedback_form);
});

// The operator's product name (Settings: product_name) on the login card, in
// the sidebar and in the tab title, and the help entries (drawHelp) in the
// sidebar, from the public /ui/brand.json. Called at
// boot (before anyone signs in) and after the setting is saved or reset.
// textContent only. Only the newest call writes: a boot read answering late
// never draws over a newer save's. On any failure the page keeps what it
// shows; index.html names no product, so a deployment's brand never flashes
// another name before the first answer.
export async function applyBrand() {
    const mine = ++brandTicket;
    let answer = null;
    try {
        const response = await fetch('/ui/brand.json', { cache: 'no-store' });
        if (response.ok) answer = await response.json();
    } catch { /* keep the current name */ }
    if (mine !== brandTicket || !answer) return;
    brand = answer;
    // Hidden when unset, also after a reset.
    drawHelp();
    if (!brand.product_name) return;
    document.title = `${brand.product_name} console`;
    for (const el of document.querySelectorAll('.brand')) el.textContent = brand.product_name;
}
