// API keys (admin): the keys this admin manages, a form to create one (its
// secret shown once), and per-key grant, ungrant, reporting and revoke.
// Below them, OAuth sign-in codes: mint one (shown once), list, revoke.
// OAuth grants (the keys codes become) are marked in the key list.
// Every key, grant and code belongs to a user, picked when it is made and
// changed (or, for one from before users existed, assigned) in its drawer.
// The server enforces every rule; this page only hides what cannot work.
// Each helper takes the page's `main` and checks `main.isConnected` after an
// await before touching the table: once the user has navigated away, the
// page is gone. A write's reload goes through `mounted` instead (below), so
// it reaches the page shown now. Each drawer holds a drawerTicket from the moment it is asked
// for: it opens, re-renders and closes only through that ticket, and only
// while it is current, so a late answer never replaces a newer drawer or
// reopens a dismissed one.
import { h, pill, fact } from '../core/dom.js';
import { api, enc } from '../core/api.js';
import { store } from '../core/store.js';
import { catalogLabel, ensureLabels, userLabel } from '../core/labels.js';
import { guard, once } from '../core/pending.js';
import { toast } from '../core/toast.js';
import { drawerTicket } from '../core/drawer.js';
import { when } from '../core/format.js';
import { listTable } from '../components/list-table.js';
import { confirmDialog } from '../components/confirm.js';
import { assignControl, picker, rowsOf, userChoices } from '../components/picker.js';
import { showOnce } from '../components/secret.js';
import { ACCESS_KIND } from '../components/levels.js';
import { keyUserChanged, refreshUnassigned } from '../core/shell.js';

// The page now mounted, if any: a write that settles after the user left this
// page and came back reloads the lists of the page shown now, not those of the
// page the write started on (which is gone, so its lists would stay stale).
let mounted = null;

// A reload function for the helpers: it reads list `which` ('keys' or 'codes')
// of the page mounted now, and does nothing once no page is shown.
const reloadMounted = which => () => (mounted?.main.isConnected ? mounted[which]() : undefined);

const LEVELS = ['read', 'write', 'admin'];
const CODE_LEVELS = ['read', 'write'];
const CODE_STATE_KIND = { live: 'ok', expired: 'muted', redeemed: 'info', revoked: 'error' };
// humantime spellings the server parses; the default is a day.
const EXPIRIES = [['1h', '1 hour', 3600], ['24h', '24 hours', 86400], ['3d', '3 days', 259200], ['7d', '7 days', 604800]];

// Per render (keyed by its `main`): the key rows last drawn and the loader
// that redraws them, so a redeemed code's drawer can open its grant's
// drawer. Scoped to the render, so a read that answers after the page was
// left and shown again cannot replace the visible page's rows or loader.
const keysViews = new WeakMap();

// The expiry choices the server accepts: those within `maxSecs`
// (`oauth.code_max_expiry`; null when unknown: all of them). When even the
// shortest is too long, the limit itself is the one choice.
function expiryChoices(maxSecs) {
    if (typeof maxSecs !== 'number') return EXPIRIES;
    const fits = EXPIRIES.filter(([, , secs]) => secs <= maxSecs);
    if (fits.length) return fits;
    // Under an hour (the shortest choice), so seconds spell it exactly.
    const value = `${maxSecs}s`;
    return [[value, value, maxSecs]];
}

// Keys and codes both carry all_catalogs and catalogs.
function catalogsOf(grantee) {
    if (grantee.all_catalogs) return pill('all', 'info');
    return grantee.catalogs.length ? grantee.catalogs.map(catalogLabel).join(', ') : '-';
}

function isMine(key) {
    return key.id === store.whoami?.key_id;
}

function isGrant(key) {
    return key.kind === 'oauth';
}

// The User cell of both tables; `prefix` is 'key' or 'code'. Its label is
// read with the rows (ensureLabels), for the users the table shows.
function userCell(prefix, row) {
    return h('span', { 'data-test': `${prefix}-user-${row.name}`, class: row.user ? 'mono' : 'muted' },
        row.user ? userLabel(row.user) : 'unassigned');
}

const COLUMNS = [
    { key: 'name', label: 'Name', sort: key => key.name,
      render: key => h('span', { class: 'key-name', 'data-test': `key-${key.name}` }, key.name,
          isMine(key) ? [' ', pill('you', 'info')] : null,
          key.provisioner ? [' ', h('span', { 'data-test': `key-provisioner-${key.name}` }, pill('provisioner', 'warn'))] : null,
          isGrant(key)
              ? [' ', pill('OAuth', 'info'), ' ', h('span', { class: 'muted small client-name' }, key.client_name ?? 'unknown client')]
              : null) },
    { key: 'actor', label: 'Actor', class: 'mono', sort: key => key.actor },
    { key: 'user', label: 'User', sort: key => key.user ?? '', render: key => userCell('key', key) },
    { key: 'access', label: 'Access', sort: key => LEVELS.indexOf(key.access),
      render: key => pill(key.access, ACCESS_KIND[key.access]) },
    { key: 'catalogs', label: 'Catalogs', render: catalogsOf },
    { key: 'default_catalog', label: 'Default', render: row => (row.default_catalog ? catalogLabel(row.default_catalog) : '-') },
    { key: 'maintenance_report', label: 'Reporting',
      render: key => key.maintenance_report ? pill('reports', 'ok') : '-' },
    { key: 'created_at', label: 'Created', sort: key => key.created_at, render: key => when(key.created_at) },
    // A grant's newest token's issue time; a plain key has none ('-').
    { key: 'last_activity', label: 'Last active', sort: key => key.last_activity ?? '',
      render: key => h('span', { 'data-test': `key-active-${key.name}` }, isGrant(key) ? when(key.last_activity) : '-') },
    { key: 'revoked_at', label: 'State', sort: key => key.revoked_at ?? '',
      render: key => key.revoked_at
          ? [pill('revoked', 'error'), ' ', h('span', { class: 'muted small' }, when(key.revoked_at))]
          : pill('active', 'ok') },
];

const CODE_COLUMNS = [
    { key: 'name', label: 'Name', sort: code => code.name,
      render: code => h('span', { class: 'key-name', 'data-test': `code-${code.name}` }, code.name) },
    { key: 'actor', label: 'Actor', class: 'mono', sort: code => code.actor },
    { key: 'user', label: 'User', sort: code => code.user ?? '', render: code => userCell('code', code) },
    { key: 'access', label: 'Access', sort: code => LEVELS.indexOf(code.access),
      render: code => pill(code.access, ACCESS_KIND[code.access]) },
    { key: 'catalogs', label: 'Catalogs', render: catalogsOf },
    { key: 'default_catalog', label: 'Default', render: row => (row.default_catalog ? catalogLabel(row.default_catalog) : '-') },
    { key: 'expires_at', label: 'Expires', sort: code => code.expires_at, render: code => when(code.expires_at) },
    { key: 'state', label: 'State', sort: code => code.state,
      render: code => h('span', { 'data-test': `code-state-${code.name}` }, pill(code.state, CODE_STATE_KIND[code.state])) },
];

export async function render(main) {
    const head = h('header', { class: 'page-head' },
        h('div', {},
            h('h1', {}, 'API keys'),
            h('p', { class: 'muted' }, 'The keys inside your reach. A secret is shown once, when its key is created.')));
    main.append(head);
    if (!store.can('admin')) {
        main.append(h('p', { class: 'muted', id: 'keys-denied' }, 'Managing keys needs an admin key.'));
        return;
    }
    const showRevoked = h('input', { type: 'checkbox', id: 'keys-show-revoked' });
    const table = listTable(COLUMNS, [], { onRow: key => showKey(main, key, reloadKeys), empty: 'No keys.' });
    table.id = 'keys-table';
    const applyFilter = () => table.setFilter(showRevoked.checked ? null : key => !key.revoked_at);
    showRevoked.addEventListener('change', applyFilter);
    head.append(h('div', { class: 'actions' },
        h('button', { type: 'button', id: 'key-new', onclick: () => newKey(main, reloadKeys) }, 'New key')));
    const reloadKeys = reloadMounted('keys');
    const view = { rows: new Map(), reload: null, latest: 0 };
    keysViews.set(main, view);
    const codes = codesPanel(main);
    main.append(h('label', { class: 'small muted' }, showRevoked, ' Show revoked keys'), table, codes.section);

    view.reload = reloadKeys;
    async function load() {
        // Numbered, so a read overtaken by a newer one draws nothing.
        const mine = ++view.latest;
        try {
            const { keys } = await api.get('/keys');
            // The labels of the users these rows name; a failed read shows
            // their slugs, never stops the rows from drawing.
            await ensureLabels({ users: keys.map(key => key.user) });
            if (!main.isConnected || mine !== view.latest) return;
            view.rows = new Map(keys.map(key => [key.id, key]));
            table.redraw(keys);
        } catch { /* toasted; a 401 has already signed the browser out */ }
    }
    applyFilter();
    mounted = { main, keys: load, codes: codes.load };
    await Promise.all([load(), codes.load()]);
}

// The catalog checkboxes and default select both forms share.
function catalogFields() {
    const me = store.whoami;
    return [
        h('fieldset', {},
            h('legend', {}, 'Catalogs'),
            me.all_catalogs
                ? h('label', {}, h('input', { type: 'checkbox', name: 'all' }), ' Every catalog, present and future')
                : null,
            me.catalogs.map(name => h('label', {},
                h('input', { type: 'checkbox', name: 'catalog', value: name }), ' ', h('span', { class: 'mono' }, name)))),
        h('label', {}, 'Default catalog',
            h('select', { name: 'default_catalog' },
                h('option', { value: '' }, 'None'),
                me.catalogs.map(name => h('option', { value: name }, name)))),
    ];
}

// The grant fields of a submitted form, as both create bodies spell them.
function readGrants(data) {
    const all = data.get('all') === 'on';
    return {
        all,
        catalogs: all ? [] : data.getAll('catalog').map(String),
        default_catalog: String(data.get('default_catalog') ?? '') || null,
    };
}

// The User picker both create forms share: required, with no default, so
// every new credential's user is a deliberate choice. `users` is
// userChoices' `{ names, labelOf }`.
function userField(users) {
    return h('label', {}, 'User', picker({ name: 'user', required: true }, users.names, null, users.labelOf));
}

// The Assign (unassigned) or Change user control of a key's or code's
// drawer. `prefix` ('key' or 'code') starts the control's ids, `path` is
// the credential's `.../user` route, `ticket` the drawer's claim, and
// `after(updated, draw)` runs with the server's answer and the drawer's
// draw number, taken as the write settled (see nextDraw); it refreshes the
// table, and the drawer while its ticket is current.
function userAssign(main, users, { prefix, row, path, ticket, after }) {
    return h('section', {},
        h('h3', {}, 'User'),
        assignControl({
            names: users.names, labelOf: users.labelOf, current: row.user, label: row.user ? 'Change user' : 'Assign',
            ids: { select: `${prefix}-user-select`, button: `${prefix}-assign` },
            assign: async user => {
                let updated;
                try { updated = await api.put(path, { user }); } catch { return; /* toasted */ }
                const draw = nextDraw(ticket);
                toast(`${row.name} now belongs to ${user}.`, 'ok');
                refreshUnassigned();
                if (prefix === 'key') await keyUserChanged(row.id);
                await after(updated, draw);
            },
        }));
}

// The users for a picker, as userChoices' `{ names, labelOf }` (labelled
// from their own rows), or null (toasted) when they cannot be read.
async function usersOrNull() {
    try { return userChoices(await rowsOf('users')); } catch { return null; }
}

// Draws of each key or sign-in code drawer, numbered per drawer (by its
// ticket): its opening, and each of its writes as it settles (before the
// table is read again), takes the next number, and it draws only while
// that number is its newest, so a write whose table read answers late
// never redraws over a newer one. Numbers are per ticket, so a write of
// a dismissed drawer never cancels another drawer's draw. A WeakMap holds
// its keys weakly: a dropped ticket's entry goes with it.
const draws = new WeakMap();

// The next draw number of the drawer `ticket`.
function nextDraw(ticket) {
    const draw = (draws.get(ticket) ?? 0) + 1;
    draws.set(ticket, draw);
    return draw;
}

// The users for a drawer drawn again after a write: read again, or, when
// that fails, the ones it was drawn with (`previous`), so a write's answer
// is never left undrawn because an unrelated read failed.
async function usersOr(previous) {
    return (await usersOrNull()) ?? previous;
}

// The create form, in the drawer.
async function newKey(main, reload) {
    const ticket = drawerTicket(main);
    const users = await usersOrNull();
    if (!users || !ticket.current()) return;
    const provisioner = guard(h('input', { type: 'checkbox', name: 'provisioner', role: 'switch' }), 'key:new', store.whoami.all_catalogs);
    const reach = h('fieldset', { class: 'key-reach' },
        h('label', {}, 'Access', h('select', { name: 'access' }, LEVELS.map(level => h('option', { value: level }, level)))),
        catalogFields(),
        h('label', {}, h('input', { type: 'checkbox', name: 'maintenance_report', role: 'switch' }), ' May submit maintenance reports'));
    provisioner.addEventListener('change', () => { reach.disabled = provisioner.checked; });
    const form = h('form', { id: 'key-form' },
        h('label', {}, 'Name', h('input', { name: 'name', required: true, autocomplete: 'off', placeholder: 'letters, digits, . _ -' })),
        h('label', {}, 'Actor', h('input', { name: 'actor', autocomplete: 'off', placeholder: 'defaults to the name' })),
        userField(users),
        h('label', {}, provisioner, ' Provisioner (no catalog access)'),
        reach,
        guard(h('button', { type: 'submit' }, 'Create key'), 'key:new'));
    form.addEventListener('submit', async event => {
        event.preventDefault();
        const data = new FormData(form);
        const body = provisioner.checked ? {
            name: String(data.get('name') ?? '').trim(),
            provisioner: true,
            actor: String(data.get('actor') ?? '').trim() || null,
            user: String(data.get('user') ?? ''),
        } : {
            name: String(data.get('name') ?? '').trim(),
            access: String(data.get('access') ?? 'read'),
            ...readGrants(data),
            actor: String(data.get('actor') ?? '').trim() || null,
            maintenance_report: data.get('maintenance_report') === 'on',
            user: String(data.get('user') ?? ''),
        };
        await once('key:new', async () => {
        try {
            const created = await api.post('/keys', body);
            // The secret is shown even after navigating away: it exists only now.
            ticket.close();
            showOnce({
                prefix: 'key',
                title: `Key ${created.key.name} created`,
                warning: 'Copy this key now. It is not stored anywhere and cannot be shown again.',
                secret: created.secret,
            });
            await reload();
        } catch { /* the server's reason is toasted; the form stays filled */ }
        });
    });
    ticket.open('New key', 'The secret is shown once, right after creation.', form);
}

// One key's drawer: its facts and user, then grant, ungrant, reporting and
// revoke. A revoked key keeps only its user control: history is attributed
// too. `ticket` is the drawer's claim: a re-render after an action passes
// the one it was opened with, so it is dropped once the drawer is replaced
// or dismissed. `previous` is the users the drawer was drawn with (see
// usersOr), on a re-render, and `draw` its number (see nextDraw).
async function showKey(main, key, reload, ticket = drawerTicket(main), previous = null, draw = nextDraw(ticket)) {
    const users = await usersOr(previous);
    if (!users || !ticket.current() || draw !== draws.get(ticket)) return;
    const me = store.whoami;
    const mine = isMine(key);
    const grant = isGrant(key);
    const facts = h('dl', { class: 'facts' },
        fact('Id', h('code', {}, key.id)),
        fact('Kind', grant ? 'OAuth grant' : 'API key'),
        grant ? fact('Client', key.client_name ?? 'unknown client') : null,
        fact('Actor', h('code', {}, key.actor)),
        fact('User', key.user ? users.labelOf(key.user) : 'unassigned'),
        fact('Access', pill(key.access, ACCESS_KIND[key.access])),
        fact('Catalogs', catalogsOf(key)),
        fact('Default', key.default_catalog ? catalogLabel(key.default_catalog) : '-'),
        fact('Reporting', key.maintenance_report ? 'may submit maintenance reports' : 'no'),
        fact('Created', when(key.created_at)),
        grant ? fact('Last active', when(key.last_activity)) : null,
        fact('Revoked', key.revoked_at ? when(key.revoked_at) : 'no'));
    const subtitle = mine
        ? 'The key this browser is signed in with'
        : grant ? `OAuth grant for ${key.client_name ?? 'an unknown client'}` : `${key.access} key`;
    const base = `/keys/${enc(key.id)}`;
    const user = userAssign(main, users, {
        prefix: 'key', row: key, path: `${base}/user`, ticket,
        after: async (updated, draw) => {
            await reload();
            if (ticket.current()) await showKey(main, updated, reload, ticket, users, draw);
        },
    });
    if (key.revoked_at) {
        ticket.open(key.name, subtitle, facts,
            h('p', { class: 'muted' }, 'This key is revoked: it authenticates nothing, and only its user can change.'),
            user);
        return;
    }
    const change = async (request, message) => {
        try {
            const updated = await request();
            const draw = nextDraw(ticket);
            toast(message, 'ok');
            await reload();
            if (ticket.current()) await showKey(main, updated, reload, ticket, users, draw);
        } catch { /* toasted */ }
    };
    let grants;
    if (key.all_catalogs) {
        grants = h('p', { class: 'muted small' }, 'Granted every catalog: per-catalog grants do not apply.');
    } else {
        const grantable = me.catalogs.filter(name => !key.catalogs.includes(name));
        const select = h('select', { id: 'key-grant-select', 'aria-label': 'Catalog to grant' },
            grantable.map(name => h('option', { value: name }, catalogLabel(name))));
        grants = h('section', {},
            h('h3', {}, 'Catalogs'),
            h('ul', { class: 'grant-list' }, key.catalogs.length
                ? key.catalogs.map(name => h('li', {},
                    h('span', {}, catalogLabel(name)), ' ',
                    h('button', {
                        type: 'button', class: 'text-btn', dataset: { ungrant: name },
                        onclick: () => change(() => api.del(`${base}/catalogs/${enc(name)}`), `Removed ${catalogLabel(name)} from ${key.name}.`),
                    }, 'Remove')))
                : h('li', { class: 'muted' }, 'No catalogs: this key reaches nothing.')),
            grantable.length
                ? h('div', { class: 'inline-form' }, select,
                    h('button', {
                        type: 'button', id: 'key-grant',
                        onclick: () => change(() => api.put(`${base}/catalogs/${enc(select.value)}`), `Granted ${select.value} to ${key.name}.`),
                    }, 'Grant'))
                : null);
    }
    // An OAuth grant never reports (the server refuses it), so no toggle.
    const report = grant ? null : h('button', {
        type: 'button', id: 'key-report-toggle', class: 'secondary',
        onclick: () => change(() => api.patch(base, { maintenance_report: !key.maintenance_report }),
            key.maintenance_report ? `${key.name} may no longer report.` : `${key.name} may now report.`),
    }, key.maintenance_report ? 'Disable reporting' : 'Enable reporting');
    const revoke = h('button', {
        type: 'button', id: 'key-revoke', class: 'danger',
        onclick: async () => {
            const ok = await confirmDialog({
                title: `Revoke ${key.name}?`,
                message: mine
                    ? 'This is the key this browser is signed in with: revoking it signs you out at once. This cannot be undone.'
                    : grant
                        ? 'The client is refused from its next request, and its tokens stop working. This cannot be undone.'
                        : 'Anything using this key is refused from its next request. This cannot be undone.',
                action: 'Revoke',
            });
            if (!ok) return;
            try {
                await api.post(`${base}/revoke`);
                toast(`Revoked ${key.name}.`, 'ok');
                ticket.close();
                await reload(); // for the signed-in key this is a 401: back to the login card
            } catch { /* toasted */ }
        },
    }, 'Revoke');
    ticket.open(key.name, subtitle, facts, user, grants, h('div', { class: 'drawer-actions' }, report, revoke));
}

// The sign-in codes section: its own head, filter and table. Returns the
// section and its loader, which `render` awaits beside the keys'.
function codesPanel(main) {
    const showAll = h('input', { type: 'checkbox', id: 'codes-show-all' });
    const table = listTable(CODE_COLUMNS, [], { onRow: code => showCode(main, code, reloadCodes), empty: 'No sign-in codes to show.' });
    table.id = 'codes-table';
    // The longest expiry the server allows, from the last list; null until
    // one has been read (the code form is not offered before that).
    let maxExpirySecs = null;
    let latest = 0;
    const reloadCodes = reloadMounted('codes');
    const applyFilter = () => table.setFilter(showAll.checked ? null : code => code.state === 'live');
    showAll.addEventListener('change', applyFilter);
    applyFilter();
    // Returns whether the list was read (so the limit is known).
    async function load() {
        const mine = ++latest;
        try {
            const { codes, max_expiry_secs: maxSecs } = await api.get('/oauth/codes');
            await ensureLabels({ users: codes.map(code => code.user) });
            if (typeof maxSecs === 'number') maxExpirySecs = maxSecs;
            if (main.isConnected && mine === latest) table.redraw(codes);
            return typeof maxSecs === 'number';
        } catch { /* toasted */ }
        return false;
    }
    // The form offers only expiries the server accepts, so it opens once the
    // limit is known: read now when the first list is still pending or failed
    // (a failure is toasted and the form stays closed; New retries it).
    async function openNew() {
        // Claimed before the read, so a drawer opened or dismissed while it
        // is pending wins: the form then does not open over or after it.
        const ticket = drawerTicket(main);
        if (maxExpirySecs === null && !(await load())) return;
        if (!ticket.current()) return;
        await newCode(main, reloadCodes, () => maxExpirySecs, ticket);
    }
    const section = h('section', { id: 'codes-panel', class: 'codes-panel' },
        h('header', { class: 'page-head' },
            h('div', {},
                h('h2', {}, 'Sign-in codes'),
                h('p', { class: 'muted' },
                    'One-time codes a person enters when connecting an OAuth client such as ChatGPT. '
                    + 'Each becomes an OAuth grant with the code\'s catalogs, access, actor and user.')),
            h('div', { class: 'actions' },
                h('button', { type: 'button', id: 'code-new', onclick: () => openNew() }, 'New sign-in code'))),
        h('label', { class: 'small muted' }, showAll, ' Show redeemed, expired and revoked codes'),
        table);
    return { section, load };
}

// The mint form, in the drawer.
async function newCode(main, reload, maxExpiry = () => null, ticket = drawerTicket(main)) {
    const users = await usersOrNull();
    if (!users || !ticket.current()) return;
    const choices = expiryChoices(maxExpiry());
    // 24 hours when allowed, else the longest choice that is.
    const preferred = (choices.find(([value]) => value === '24h') ?? choices[choices.length - 1])[0];
    const form = h('form', { id: 'code-form' },
        h('label', {}, 'Name', h('input', { name: 'name', required: true, autocomplete: 'off', placeholder: 'letters, digits, . _ -' })),
        h('label', {}, 'Actor', h('input', { name: 'actor', autocomplete: 'off', placeholder: 'defaults to the name' })),
        userField(users),
        h('label', {}, 'Access', h('select', { name: 'access' }, CODE_LEVELS.map(level => h('option', { value: level }, level)))),
        catalogFields(),
        h('label', {}, 'Expires unused after',
            h('select', { name: 'expires_in' },
                choices.map(([value, label]) => h('option', { value, selected: value === preferred }, label)))),
        h('button', { type: 'submit' }, 'Create sign-in code'));
    form.addEventListener('submit', async event => {
        event.preventDefault();
        const data = new FormData(form);
        const body = {
            name: String(data.get('name') ?? '').trim(),
            actor: String(data.get('actor') ?? '').trim() || null,
            access: String(data.get('access') ?? 'read'),
            ...readGrants(data),
            expires_in: String(data.get('expires_in') ?? preferred),
            user: String(data.get('user') ?? ''),
        };
        try {
            const created = await api.post('/oauth/codes', body);
            ticket.close();
            showOnce({
                prefix: 'code',
                title: `Sign-in code ${created.code.name} created`,
                warning: 'Give this code to the person connecting now. It works once, is not stored anywhere and cannot be shown again.',
                secret: created.secret,
            });
            await reload();
        } catch { /* the server's reason is toasted; the form stays filled */ }
    });
    ticket.open('New sign-in code', 'The code is shown once, right after creation.', form);
}

// A redeemed code's grant: a button that opens the grant's drawer when the
// key list holds it (a grant outside the caller's reach is not listed), else
// the id as text.
function grantLink(main, grantId) {
    const id = h('code', {}, grantId);
    const view = keysViews.get(main);
    const key = view?.rows.get(grantId);
    if (!key || !view.reload) return id;
    return h('button', {
        type: 'button', class: 'outline', id: 'code-open-grant', title: 'Open this grant',
        onclick: () => showKey(main, key, view.reload),
    }, id);
}

// One code's drawer: its facts and user, and Revoke while nobody has
// redeemed it. A redeemed code's grant keeps its own user: change that in
// the grant's drawer. `ticket`, `previous` and `draw` as for showKey.
async function showCode(main, code, reload, ticket = drawerTicket(main), previous = null, draw = nextDraw(ticket)) {
    const users = await usersOr(previous);
    if (!users || !ticket.current() || draw !== draws.get(ticket)) return;
    const facts = h('dl', { class: 'facts' },
        fact('Id', h('code', {}, code.id)),
        fact('Actor', h('code', {}, code.actor)),
        fact('User', code.user ? users.labelOf(code.user) : 'unassigned'),
        fact('Access', pill(code.access, ACCESS_KIND[code.access])),
        fact('Catalogs', catalogsOf(code)),
        fact('Default', code.default_catalog ? catalogLabel(code.default_catalog) : '-'),
        fact('State', pill(code.state, CODE_STATE_KIND[code.state])),
        fact('Created', when(code.created_at)),
        fact('Expires', when(code.expires_at)),
        code.redeemed_at ? fact('Redeemed', when(code.redeemed_at)) : null,
        code.grant_id ? fact('Grant', grantLink(main, code.grant_id)) : null,
        code.revoked_at ? fact('Revoked', when(code.revoked_at)) : null);
    const revocable = code.state === 'live' || code.state === 'expired';
    const revoke = revocable ? h('button', {
        type: 'button', id: 'code-revoke', class: 'danger',
        onclick: async () => {
            const ok = await confirmDialog({
                title: `Revoke sign-in code ${code.name}?`,
                message: 'Nobody can redeem it afterwards. This cannot be undone.',
                action: 'Revoke',
            });
            if (!ok) return;
            try {
                await api.del(`/oauth/codes/${enc(code.id)}`);
                toast(`Revoked sign-in code ${code.name}.`, 'ok');
                ticket.close();
                await reload();
            } catch { /* toasted */ }
        },
    }, 'Revoke') : null;
    const user = userAssign(main, users, {
        prefix: 'code', row: code, path: `/oauth/codes/${enc(code.id)}/user`, ticket,
        after: async (updated, draw) => {
            await reload();
            if (ticket.current()) await showCode(main, updated, reload, ticket, users, draw);
        },
    });
    ticket.open(code.name, 'Sign-in code', facts, user,
        code.state === 'redeemed'
            ? h('p', { class: 'muted small' }, 'Redeemed: revoke the grant it became in the key list above.')
            : null,
        h('div', { class: 'drawer-actions' }, revoke));
}
