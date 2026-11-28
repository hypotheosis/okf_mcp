// My keys: the API keys that belong to the signed-in person. New key picks
// catalogs and read or write for each, all within the account's ceiling
// (found a page at a time with a Find box, components/ceiling.js: never
// the whole ceiling, issue #736; ticks stay across finds),
// and an expiry no longer than the longest allowed; the secret is shown
// once. Each key's drawer renames or revokes it. Keys an admin made for
// this user are listed too, uncapped. The server enforces the ceiling, the
// key limit and the lifetime limit (422, naming the limit); this page only
// offers what fits. Each drawer keeps the drawerTicket it was asked for, as
// on the API keys page.
import { h, pill, fact, fill } from '../core/dom.js';
import { api, enc } from '../core/api.js';
import { catalogLabel } from '../core/labels.js';
import { toast } from '../core/toast.js';
import { drawerTicket } from '../core/drawer.js';
import { latest } from '../core/latest.js';
import { count, when } from '../core/format.js';
import { freshAccount, requireSession } from '../core/shell.js';
import { listTable } from '../components/list-table.js';
import { confirmDialog } from '../components/confirm.js';
import { showOnce } from '../components/secret.js';
import { levelsCell } from '../components/levels.js';
import { ceilingPager } from '../components/ceiling.js';
import { guard, once } from '../core/pending.js';

const LEVELS = ['read', 'write'];
const STATE_KIND = { active: 'ok', expired: 'muted', revoked: 'error' };
// Expiry choices: [the select's value, label, seconds].
const EXPIRIES = [
    ['7d', '7 days', 604800],
    ['30d', '30 days', 2592000],
    ['90d', '90 days', 7776000],
    ['365d', '1 year', 31536000],
];
// The select's value for "the longest allowed": sent as no expiry, which
// the server turns into the `max_key_lifetime` limit itself.
const LONGEST = 'longest';

// A KeyView's state: revoked, expired (by its `expires_at`), or active.
function keyState(key) {
    if (key.revoked_at) return 'revoked';
    if (key.expires_at && Date.parse(key.expires_at) <= Date.now()) return 'expired';
    return 'active';
}

// The account's ceiling on the catalogs of its live self-issued keys, as
// the last GET /me/keys gave it ({catalog: level}): what the cap notes
// compare with.
let keysCeiling = {};

const COLUMNS = [
    { key: 'name', label: 'Name', sort: key => key.name,
      render: key => h('span', { class: 'key-name', dataset: { test: `my-key-${key.name}` } }, key.name) },
    { key: 'catalogs', label: 'Catalogs', render: key => levelsCell(key, keysCeiling) },
    { key: 'default_catalog', label: 'Default', render: row => (row.default_catalog ? catalogLabel(row.default_catalog) : '-') },
    { key: 'created_at', label: 'Created', sort: key => key.created_at, render: key => when(key.created_at) },
    { key: 'expires_at', label: 'Expires', sort: key => key.expires_at ?? '9999',
      render: key => key.expires_at ? when(key.expires_at) : 'never' },
    { key: 'state', label: 'State', sort: keyState,
      render: key => h('span', { dataset: { test: `my-key-state-${key.name}` } }, pill(keyState(key), STATE_KIND[keyState(key)])) },
];

export async function render(main) {
    const head = h('header', { class: 'page-head' },
        h('div', {},
            h('h1', {}, 'My keys'),
            h('p', { class: 'muted' },
                'API keys that belong to you. Keys you create stay within your ceiling; a secret is shown once, when its key is created.')));
    main.append(head);
    if (!requireSession(main, 'my-keys-denied')) return;
    const create = h('button', { type: 'button', id: 'my-key-new', disabled: true }, 'New key');
    const note = h('p', { id: 'my-keys-note', class: 'muted small' });
    const showOld = h('input', { type: 'checkbox', id: 'my-keys-show-old' });
    const table = listTable(COLUMNS, [], { onRow: key => showKey(main, key, load), empty: 'You have no keys yet.' });
    table.id = 'my-keys-table';
    const applyFilter = () => table.setFilter(showOld.checked ? null : key => keyState(key) === 'active');
    showOld.addEventListener('change', applyFilter);
    applyFilter();
    head.append(h('div', { class: 'actions' }, create));
    main.append(note, h('label', { class: 'small muted' }, showOld, ' Show revoked and expired keys'), table);
    let account = null;
    create.addEventListener('click', () => newKey(main, account, load));

    // Numbers the list's reads: a slower older read never draws over a
    // newer one, and none draws once the page was left (core/latest.js).
    const reads = latest(main);
    async function load() {
        const current = reads();
        let fresh;
        let keys;
        let ceiling;
        try {
            // /me/keys' `ceiling` is the account's ceiling on these keys'
            // catalogs now, for the cap notes; /me's `ceiling_access` says
            // whether the ceiling reaches anything.
            [fresh, { keys, ceiling }] = await Promise.all([freshAccount(), api.get('/me/keys')]);
        } catch { return; /* toasted */ }
        if (!current()) return;
        // Only the newest read sets the snapshot the New key form uses.
        account = fresh;
        keysCeiling = ceiling ?? {};
        table.redraw(keys);
        // What `max_keys_per_user` counts: live self-issued keys.
        const live = keys.filter(key => key.self_issued && keyState(key) === 'active').length;
        const reason = createBlocker(account, live);
        create.disabled = !!reason;
        note.textContent = reason || `${live} of ${account.limits.max_keys_per_user} keys in use.`;
    }
    await load();
}

// Why New key is unavailable, or '' when it is not.
function createBlocker({ ceiling_access: ceilingAccess, limits }, live) {
    if (ceilingAccess === null) {
        return 'Your account reaches no catalog yet, so a key could reach nothing. Ask an admin for access.';
    }
    if (live >= limits.max_keys_per_user) {
        return `You have ${count(live, 'live key')}, the most allowed. Revoke one first.`;
    }
    return '';
}

// The levels up to and including `access` (the ceiling's on a catalog).
function levelsUpTo(access) {
    return LEVELS.slice(0, LEVELS.indexOf(access) + 1);
}

// A lifetime in seconds, in words.
function lifetime(secs) {
    return secs % 86400 === 0 ? count(secs / 86400, 'day') : count(Math.round(secs / 3600), 'hour');
}

// The Expires options: "Never" and every choice when there is no limit;
// otherwise the choices within it, plus the limit itself (preselected),
// so the longest allowed is always on offer.
function expiryOptions({ max_key_lifetime_secs: longestSecs }) {
    if (longestSecs === null) {
        return [h('option', { value: '', selected: true }, 'Never'),
            ...EXPIRIES.map(([value, label]) => h('option', { value }, label))];
    }
    const fitting = EXPIRIES.filter(([, , secs]) => secs < longestSecs);
    return [...fitting.map(([value, label]) => h('option', { value }, label)),
        h('option', { value: LONGEST, selected: true }, `${lifetime(longestSecs)} (the longest allowed)`)];
}

// The body's `expires_at` (RFC 3339) for a choice, or null for "Never" and
// "the longest allowed" (the server then applies the limit, if any).
function expiresAt(choice) {
    const entry = EXPIRIES.find(([value]) => value === choice);
    return entry ? new Date(Date.now() + entry[2] * 1000).toISOString() : null;
}

// The create form, in the drawer: the ceiling a page at a time (a Find box
// and Load more), one row per catalog, each a checkbox and a level select
// offering no more than the ceiling. What is ticked is kept in `chosen`
// (catalog to level), not in the rows, so it stays across finds and pages;
// the Default catalog offers the chosen ones. The create is one pending
// write (core/pending.js).
function newKey(main, account, reload) {
    if (!account) return;
    const ticket = drawerTicket(main);
    const chosen = new Map();
    const rows = h('div', { id: 'my-key-catalogs' });
    const summary = h('p', { id: 'my-key-chosen', class: 'muted small' });
    const defaultSelect = h('select', { name: 'default_catalog', id: 'my-key-default' });

    // The chosen catalogs, in words, and the Default options: kept when
    // still chosen.
    function showChosen() {
        const names = [...chosen.keys()].sort();
        summary.textContent = names.length
            ? `Chosen: ${names.map(name => `${catalogLabel(name)} (${chosen.get(name)})`).join(', ')}`
            : 'No catalog chosen yet.';
        const current = defaultSelect.value;
        defaultSelect.replaceChildren(h('option', { value: '' }, 'None'),
            ...names.map(name => h('option', { value: name }, catalogLabel(name))));
        defaultSelect.value = chosen.has(current) ? current : '';
    }

    function row({ catalog, access }) {
        const box = h('input', { type: 'checkbox', name: 'catalog', value: catalog, checked: chosen.has(catalog) });
        const level = h('select', { name: `level-${catalog}`, 'aria-label': `Access to ${catalogLabel(catalog)}` },
            levelsUpTo(access).map(option => h('option', { value: option }, option)));
        // A level chosen earlier. One the ceiling no longer allows (lowered
        // since it was ticked) is lowered to the most it allows now, in
        // `chosen` too, so Create sends what the select shows; the draw
        // below shows the change in the summary.
        if (chosen.has(catalog)) {
            if (!levelsUpTo(access).includes(chosen.get(catalog))) chosen.set(catalog, access);
            level.value = chosen.get(catalog);
        }
        const update = () => {
            if (box.checked) chosen.set(catalog, level.value);
            else chosen.delete(catalog);
            showChosen();
        };
        box.addEventListener('change', update);
        level.addEventListener('change', () => { if (box.checked) update(); });
        return h('div', { class: 'inline-form', dataset: { test: `my-key-catalog-${catalog}` } },
            h('label', {}, box, ' ', h('span', {}, catalogLabel(catalog))), level);
    }

    const pager = ceilingPager({ id: 'my-key-ceiling', draw: (entries, prefix) => {
        fill(rows, entries.length
            ? entries.map(row)
            : h('p', { class: 'muted' }, prefix ? `No catalog of yours starts with ${prefix}.` : 'Your account reaches no catalog yet.'));
        // `row` may have lowered a chosen level to the ceiling's now.
        showChosen();
    } });
    const key = 'my-key:create';
    const submit = guard(h('button', { type: 'submit', id: 'my-key-create' }, 'Create key'), key);
    const form = h('form', { id: 'my-key-form' },
        h('label', {}, 'Name',
            h('input', { name: 'name', required: true, autocomplete: 'off', placeholder: 'letters, digits, . _ -' })),
        h('fieldset', {}, h('legend', {}, 'Catalogs, each read or write within your ceiling'),
            h('label', { class: 'small' }, 'Find ', pager.find), rows, pager.footer, summary),
        h('label', {}, 'Default catalog', defaultSelect),
        h('label', {}, 'Expires', h('select', { name: 'expires_in' }, expiryOptions(account.limits))),
        submit);
    showChosen();
    form.addEventListener('submit', event => {
        event.preventDefault();
        once(key, () => create());
    });

    async function create() {
        const data = new FormData(form);
        const catalogs = [...chosen].map(([catalog, access]) => ({ catalog, access }));
        const defaultCatalog = defaultSelect.value || null;
        if (!catalogs.length) {
            toast('Choose at least one catalog.', 'error');
            return;
        }
        let created;
        try {
            // POST /me/keys' body: {name, catalogs, default_catalog, actor, expires_at}.
            created = await api.post('/me/keys', {
                name: String(data.get('name') ?? '').trim(),
                catalogs,
                default_catalog: defaultCatalog,
                expires_at: expiresAt(String(data.get('expires_in') ?? '')),
            });
        } catch { return; /* toasted: the 422 says which limit; the form stays filled */ }
        // The secret is shown even after navigating away: it exists only now.
        ticket.close();
        showOnce({
            prefix: 'my-key',
            title: `Key ${created.key.name} created`,
            warning: 'Copy this key now. It is not stored anywhere and cannot be shown again.',
            secret: created.secret,
        });
        if (main.isConnected) await reload();
    }
    ticket.open('New key', 'The secret is shown once, right after creation.', form);
    pager.start();
}

// One key's drawer: its facts, then Rename and Revoke while it is active.
// `ticket` is the drawer's claim; a re-render after a rename passes the one
// it was opened with.
function showKey(main, key, reload, ticket = drawerTicket(main)) {
    const base = `/me/keys/${enc(key.id)}`;
    const state = keyState(key);
    const facts = h('dl', { class: 'facts' },
        fact('Id', h('code', {}, key.id)),
        fact('Actor', h('code', {}, key.actor)),
        fact('Catalogs', levelsCell(key, keysCeiling)),
        fact('Default', key.default_catalog ? catalogLabel(key.default_catalog) : '-'),
        fact('Made by', key.self_issued ? 'you' : 'an admin (not capped by your ceiling)'),
        fact('Created', when(key.created_at)),
        fact('Expires', key.expires_at ? when(key.expires_at) : 'never'),
        fact('State', pill(state, STATE_KIND[state])),
        key.revoked_at ? fact('Revoked', when(key.revoked_at)) : null);
    if (state !== 'active') {
        ticket.open(key.name, `${state} key`, facts);
        return;
    }
    const name = h('input', { name: 'name', value: key.name, required: true, autocomplete: 'off', 'aria-label': 'New name' });
    const rename = h('form', { id: 'my-key-rename-form', class: 'inline-form' },
        name, h('button', { type: 'submit' }, 'Rename'));
    rename.addEventListener('submit', async event => {
        event.preventDefault();
        let updated;
        try { updated = await api.patch(base, { name: name.value.trim() }); } catch { return; /* toasted */ }
        toast(`Renamed ${key.name} to ${updated.name}.`, 'ok');
        if (main.isConnected) await reload();
        if (ticket.current()) showKey(main, updated, reload, ticket);
    });
    const revoke = h('button', {
        type: 'button', id: 'my-key-revoke', class: 'danger',
        onclick: async () => {
            const ok = await confirmDialog({
                title: `Revoke ${key.name}?`,
                message: 'Anything using this key is refused from its next request. This cannot be undone.',
                action: 'Revoke',
            });
            if (!ok) return;
            // DELETE /me/keys/{id} revokes (the row stays, revoked).
            try { await api.del(base); } catch { return; /* toasted */ }
            toast(`Revoked ${key.name}.`, 'ok');
            ticket.close();
            if (main.isConnected) await reload();
        },
    }, 'Revoke');
    ticket.open(key.name, 'Your key', facts,
        h('section', {}, h('h3', {}, 'Name'), rename),
        h('div', { class: 'drawer-actions' }, revoke));
}
