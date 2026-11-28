// Catalogs: the catalogs this key reaches, with their owner teams. Admins
// edit descriptions and delete; an admin key that reaches every catalog
// also creates them and changes their owner (plan Decision 3). Anyone may
// open a catalog's drawer for its maintenance switch, which an every-catalog
// admin and the owners of the catalog's team change (M14 Task 9e). An owner
// of a shared team also sets the team's member access and entitles its
// members from the drawer (issue #352), in a console session only. An admin
// also sees which store each catalog is on: a column that sorts by store, a
// filter, the location as a tooltip and in the drawer, and a notice naming
// the store of any catalog left out because its store is down (issue #1030).
// The API sends `store` to admins only, so every use below tolerates its
// absence.
import { $, h, fill, pill } from '../core/dom.js';
import { api, enc } from '../core/api.js';
import { store } from '../core/store.js';
import { toast } from '../core/toast.js';
import { drawerTicket } from '../core/drawer.js';
import { ago, count } from '../core/format.js';
import { listTable } from '../components/list-table.js';
import { confirmDialog } from '../components/confirm.js';
import { rowsOf, teamChoices, picker, assignControl } from '../components/picker.js';
import { canAdministerPeople, freshAccount, refreshIdentity, refreshUnassigned } from '../core/shell.js';
import { guard, isPending, once, recheck } from '../core/pending.js';
import { named } from '../core/labels.js';

// The catalogs page now mounted, if any: a create or delete that settles after
// the user left this page and came back reloads the page shown now, not the
// one the write started on (which is gone).
let mounted = null;

// Where a store is, as text: `account database`, or `host:port/database`.
// The server gives an admin exactly the fields `GET /stores` shows.
function storeLocation(place) {
    return place.location === 'host' ? `${place.host}:${place.port}/${place.database}` : 'account database';
}

export async function render(main) {
    // Team name -> true when personal, from the last read of GET /teams
    // (admins only; empty until then or when it failed), so the Owner column
    // can say what kind of team owns a catalog.
    const personalTeams = new Map();
    const admin = store.can('admin');
    const creator = admin && store.whoami.all_catalogs;
    const columns = [
        { key: 'name', label: 'Name', sort: row => row.label.toLowerCase(),
          render: row => h('span', { dataset: { catalog: row.name } }, named(row.label, row.name),
              // `suspended` is {team, notice} while its owner team is suspended.
              row.suspended
                  ? [' ', h('span', { dataset: { test: `catalog-suspended-${row.name}` },
                                     title: row.suspended.notice ?? `Team ${row.suspended.team} is suspended.` },
                            pill('suspended', 'error'))]
                  : null) },
        { key: 'description', label: 'Description',
          render: row => row.description || h('span', { class: 'muted' }, '-') },
        { key: 'owner_team', label: 'Owner', sort: row => row.owner_team_label ?? '',
          render: row => h('span', { dataset: { test: `catalog-owner-${row.name}` },
                                     class: row.owner_team ? null : 'muted' },
              row.owner_team_label ?? row.owner_team ?? 'unassigned',
              personalTeams.has(row.owner_team)
                  ? [' ', pill(personalTeams.get(row.owner_team) ? 'personal team' : 'team')]
                  : null) },
        ...(admin ? [{ key: 'store', label: 'Store', sort: row => row.store?.name ?? '',
          render: row => row.store
              ? h('span', { class: 'mono', title: storeLocation(row.store),
                            dataset: { test: `catalog-store-${row.name}` } }, row.store.name)
              : h('span', { class: 'muted' }, '-') }] : []),
        { key: 'concepts', label: 'Concepts', sort: row => row.concepts, class: 'num' },
        { key: 'files', label: 'Files', sort: row => row.files, class: 'num' },
        { key: 'generation', label: 'Generation', sort: row => row.generation, class: 'num' },
        { key: 'last_change', label: 'Last change', sort: row => row.last_change ?? '',
          render: row => ago(row.last_change) },
    ];
    // The store filter (admins): narrows the rows drawn to one store. Its
    // options are the stores of the rows loaded, rebuilt after every draw
    // with the choice kept while that store still has a row.
    const storeFilter = admin
        ? h('select', { id: 'catalogs-store-filter', 'aria-label': 'Filter catalogs by store' },
            h('option', { value: '' }, 'All stores'))
        : null;
    const table = listTable(columns, [], {
        onRow: admin ? openEdit : openOwner,
        empty: () => (storeFilter?.value ? 'No catalog on this store.' : 'This key reaches no catalog.'),
    });
    storeFilter?.addEventListener('change', () => {
        const chosen = storeFilter.value;
        table.setFilter(chosen ? row => row.store?.name === chosen : null);
    });
    function drawStoreFilter() {
        if (!storeFilter) return;
        const chosen = storeFilter.value;
        const names = [...new Set(rows.map(row => row.store?.name).filter(Boolean))].sort();
        storeFilter.replaceChildren(h('option', { value: '' }, 'All stores'),
            ...names.map(name => h('option', { value: name }, name)));
        storeFilter.value = names.includes(chosen) ? chosen : '';
        // A store that has no row any more is no longer a filter.
        if (storeFilter.value !== chosen) table.setFilter(null);
    }
    // Catalogs left out because their store cannot be reached, by catalog
    // (the answer's `unavailable_stores`, sent to admins): the notice names
    // each such store with its catalogs. Reset by a first page, added to by
    // a continuation.
    const down = new Map();
    const downNote = h('p', { id: 'catalogs-unavailable', class: 'error small', role: 'status', hidden: true });
    function drawDown() {
        const byStore = new Map();
        for (const [catalog, name] of down) byStore.set(name, [...(byStore.get(name) ?? []), catalog]);
        downNote.hidden = byStore.size === 0;
        downNote.textContent = [...byStore].sort(([a], [b]) => (a < b ? -1 : 1))
            .map(([name, catalogs]) => `Store ${name} cannot be reached; left out: ${catalogs.sort().join(', ')}.`)
            .join(' ');
    }
    const noteDown = answer => {
        for (const [catalog, name] of Object.entries(answer.unavailable_stores ?? {})) down.set(catalog, name);
        drawDown();
    };

    // What is drawn: the rows and the cursor that continues them (GET
    // /catalogs answers one page, with `next` while more follow). Only a
    // first page that draws replaces both, and only a continuation that
    // appends advances the cursor; a failed or overtaken read leaves them as
    // they are, so the rows on screen keep their Load more (a pending read
    // only hides the button). Reads are numbered: a slower older read never
    // draws over a newer one.
    let rows = [];
    let cursor = null;
    let loads = 0;           // the number of the newest first-page load
    let answered = 0;        // the number of the newest load that answered
    let continuing = null;   // the load whose rows a continuation is pending for
    let loadFailed = false;  // the newest load failed over the rows drawn
    let moreFailed = false;  // the last continuation of the rows drawn failed
    const more = h('button', { type: 'button', id: 'catalogs-more', class: 'secondary', hidden: true,
                               onclick: () => loadMore() }, 'Load more');
    const stale = h('p', { id: 'catalogs-stale', class: 'error small', role: 'status', hidden: true },
        'The catalogs below may be out of date.');

    // Load more shows while the rows drawn have a cursor and nothing is
    // pending: no newer load (whose rows would replace them) and no
    // continuation of these rows (a second click would append it twice).
    function showMore() {
        more.hidden = cursor === null || answered !== loads || continuing === loads;
        stale.hidden = !((loadFailed || moreFailed) && rows.length);
    }

    async function load() {
        const mine = ++loads;
        showMore();
        // Admins read the teams too, for the Owner column's kind; a failed
        // read only leaves the kinds out.
        const kinds = admin
            ? rowsOf('teams').then(rows => rows.map(row => [row.name, Boolean(row.personal_of)]), () => [])
            : Promise.resolve([]);
        let answer, teamKinds;
        try {
            [answer, teamKinds] = await Promise.all([api.get('/catalogs'), kinds]);
        } catch {
            // Toasted. The rows drawn keep their cursor, marked out of date.
            if (mine !== loads || !main.isConnected) return;
            answered = mine;
            loadFailed = true;
            showMore();
            return;
        }
        if (mine !== loads || !main.isConnected) return;   // overtaken
        answered = mine;
        personalTeams.clear();
        for (const [name, personal] of teamKinds) personalTeams.set(name, personal);
        rows = answer.catalogs;
        cursor = answer.next ?? null;
        loadFailed = false;
        moreFailed = false;
        down.clear();
        noteDown(answer);
        drawStoreFilter();
        table.redraw(rows);
        showMore();
    }

    // The page after the rows drawn. It appends only while no newer load
    // has started; a failure keeps the cursor for a retry.
    async function loadMore() {
        if (cursor === null || answered !== loads || continuing === loads) return;
        const mine = loads;
        continuing = mine;
        showMore();
        let page;
        try {
            page = await api.get(`/catalogs?${new URLSearchParams({ after: cursor })}`);
        } catch {
            if (mine !== loads || !main.isConnected) return;
            continuing = null;
            moreFailed = true;
            showMore();
            return;
        }
        if (mine !== loads || !main.isConnected) return;
        continuing = null;
        rows = rows.concat(page.catalogs);
        cursor = page.next ?? null;
        moreFailed = false;
        noteDown(page);
        drawStoreFilter();
        table.redraw(rows);
        showMore();
    }

    // After a create or delete: the switcher is shell state and must follow
    // the change even if the user has navigated away meanwhile, so it is
    // rebuilt first, before the table. The drawer is closed only while
    // `ticket` (the one it was opened with) is current: once replaced or
    // dismissed it is another drawer, or none. The list reloaded is the one
    // mounted now (see `mounted`).
    async function settled(ticket) {
        await refreshIdentity().catch(() => {});
        refreshUnassigned();   // a deleted catalog may have been unassigned
        ticket.close();
        if (mounted?.main.isConnected) await mounted.load();
    }

    function deletingText(row) {
        return `Deleting ${row.label}. Its data is removed in the background; this can take a while, and you can keep using the console.`;
    }

    // Sets the note of the delete `key` in every drawer that shows it (hidden
    // when `text` is empty), so a drawer drawn again while the write runs, or
    // after it failed, says the same as the one it started in.
    function showDeleteNote(key, text, failed) {
        document.querySelectorAll('[data-delete-note]').forEach(el => {
            if (el.dataset.deleteNote !== key) return;
            el.textContent = text;
            el.hidden = !text;
            el.className = failed ? 'error small' : 'muted small';
        });
    }

    async function openCreate() {
        const ticket = drawerTicket(main);
        let teams;
        try { teams = teamChoices(await rowsOf('teams')); } catch { return; }
        if (!ticket.current()) return;
        const form = h('form', { id: 'catalog-create-form' },
            h('label', {}, 'Name',
                h('input', { name: 'name', required: true, pattern: '[a-z0-9-]+', autocomplete: 'off',
                             placeholder: 'lowercase letters, digits and -' })),
            h('label', {}, 'Description', h('textarea', { name: 'description', rows: '3' })),
            h('label', {}, 'Owner team', picker({ name: 'owner', required: true }, teams.names, null, teams.labelOf)),
            h('button', { type: 'submit' }, 'Create catalog'));
        form.addEventListener('submit', async event => {
            event.preventDefault();
            const data = new FormData(form);
            const name = String(data.get('name') ?? '').trim();
            try {
                await api.post('/catalogs', { name, description: String(data.get('description') ?? ''),
                                              owner: String(data.get('owner') ?? '') });
            } catch { return; /* toasted: invalid or taken name, or unknown owner */ }
            toast(`Created catalog ${name}.`, 'ok');
            await settled(ticket);
        });
        ticket.open('New catalog', 'Starts with an index.md and a log.md; agents can add to it at once, or import a .tar.gz into it next.', form);
    }

    async function openEdit(row) {
        const ticket = drawerTicket(main);
        let teams;
        try { teams = teamChoices(await rowsOf('teams')); } catch { return; }
        if (!ticket.current()) return;
        const edit = detailsEditor(row, ticket);
        const owner = h('section', {},
            h('h3', {}, 'Owner'),
            assignControl({
                names: teams.names, labelOf: teams.labelOf, current: row.owner_team, label: row.owner_team ? 'Change owner' : 'Assign',
                allowed: creator, ids: { select: 'catalog-owner-select', button: 'catalog-owner-save' },
                assign: async team => {
                    try {
                        await api.put(`/catalogs/${enc(row.name)}/owner`, { team });
                    } catch { return; /* toasted: an unknown team */ }
                    toast(`${row.label} is now owned by ${team}.`, 'ok');
                    refreshUnassigned();
                    ticket.close();
                    if (main.isConnected) await load();
                },
            }),
            creator ? null : h('p', { class: 'muted small' }, 'Changing the owner needs an admin key that reaches every catalog.'));
        // A pending write keyed by the catalog (core/pending.js): while the
        // DELETE runs, Delete and the force box are disabled in every drawer
        // that shows this catalog, one drawn again included, and a second
        // start is ignored. The note says what is running, or why the last
        // try failed, and the outcome is toasted whichever page is shown.
        const deleting = `catalog:${row.name}:delete`;
        const force = guard(h('input', { type: 'checkbox', id: 'catalog-force' }), deleting);
        const remove = guard(h('button', { type: 'button', id: 'catalog-delete', class: 'danger' }, 'Delete catalog'), deleting);
        const note = h('p', { id: 'catalog-delete-note', class: 'muted small', role: 'status',
            hidden: !isPending(deleting) }, isPending(deleting) ? deletingText(row) : '');
        note.dataset.deleteNote = deleting;
        remove.addEventListener('click', async () => {
            const ok = await confirmDialog({
                title: `Delete ${row.label}?`,
                message: 'Its documents, history, changesets and key grants are deleted. This cannot be undone.',
                action: 'Delete',
                typed: row.name,
            });
            if (!ok) return;
            await once(deleting, async () => {
                showDeleteNote(deleting, deletingText(row), false);
                const withForce = force.checked;
                try {
                    await api.del(`/catalogs/${enc(row.name)}${withForce ? '?force=true' : ''}`);
                } catch (error) {
                    // Toasted by api.js (409 names ?force=true); every drawer
                    // of this catalog keeps the reason, with its controls usable.
                    showDeleteNote(deleting, `Could not delete ${row.label}: ${error.message}`, true);
                    return;
                }
                toast(`Deleted catalog ${row.label}.`, 'ok');
                showDeleteNote(deleting, '', false);
                await settled(ticket);
            });
        });
        const members = memberAccess(row, creator, ticket);
        const switcher = maintenance(row, await maySwitchMaintenance(row), ticket);
        if (!ticket.current()) return;
        ticket.open(row.label, subtitle(row),
            edit,
            storeSection(row),
            owner,
            members.section,
            switcher.section,
            h('section', { class: 'danger-zone' },
                h('h3', {}, 'Delete'),
                h('label', {}, force, ' Also discard unpublished changes (force)'),
                remove,
                note));
        await Promise.all([members.refresh(), switcher.refresh()]);
    }

    // The Display name and Description form, shared by the admin and the
    // owner drawer. It sends only the fields that changed (PATCH
    // /catalogs/{name}), toasts the new label, reads the list and the
    // identity (the switcher's labels) again, and closes the drawer. It
    // needs no team list, which only an admin may read. Saving is a pending
    // write keyed by the catalog (core/pending.js).
    function detailsEditor(row, ticket) {
        const key = `catalog:${row.name}:details`;
        const displayName = guard(h('input', { id: 'catalog-display-name', name: 'display_name',
            value: row.display_name, autocomplete: 'off' }), key);
        const description = guard(h('textarea', { id: 'catalog-description', name: 'description', rows: '3' },
            row.description), key);
        const save = guard(h('button', { type: 'submit', id: 'catalog-save' }, 'Save'), key);
        const form = h('form', { id: 'catalog-edit-form' },
            h('label', {}, 'Display name', displayName),
            h('label', {}, 'Description', description),
            save);
        form.addEventListener('submit', event => {
            event.preventDefault();
            const change = {};
            if (displayName.value.trim() !== row.display_name) change.display_name = displayName.value.trim();
            if (description.value !== row.description) change.description = description.value;
            if (!Object.keys(change).length) {
                toast('Nothing changed.', 'ok');
                return;
            }
            once(key, async () => {
                let answer;
                try {
                    answer = await api.patch(`/catalogs/${enc(row.name)}`, change);
                } catch { return; /* toasted: a refused display name, or not an owner */ }
                toast(`Saved ${answer.label}.`, 'ok');
                await refreshIdentity().catch(() => {});
                ticket.close();
                if (main.isConnected) await load();
            });
        });
        return form;
    }

    // Someone who administers no catalog: the drawer holds the maintenance
    // switch, and above it the details editor for an owner of the catalog's
    // team (never the owner picker or Delete, which are the admin's). An
    // owner of a shared team also gets the team's member access and the
    // entitlements of its members (issue #352).
    async function openOwner(row) {
        const ticket = drawerTicket(main);
        const [allowed, sharing] = await Promise.all([maySwitchMaintenance(row), maySetTeamAccess(row)]);
        if (!ticket.current()) return;
        const switcher = maintenance(row, allowed, ticket);
        const members = sharing ? memberAccess(row, true, ticket) : null;
        const given = sharing ? entitlements(row, ticket) : null;
        ticket.open(row.label, subtitle(row), allowed ? detailsEditor(row, ticket) : null,
            members?.section ?? null, given?.section ?? null, switcher.section);
        await Promise.all([switcher.refresh(), members?.refresh(), given?.refresh()]);
    }

    // fill(), not native append: the note may be null (issue #302).
    fill(main,
        h('div', { class: 'page-head' },
            h('div', {}, h('h1', {}, 'Catalogs'), h('p', { class: 'muted' }, 'The catalogs this key reaches.')),
            h('div', { class: 'actions' },
                storeFilter,
                creator ? h('button', { type: 'button', id: 'catalog-create', onclick: openCreate }, 'New catalog') : null)),
        downNote,
        table,
        stale,
        more,
        admin && !creator
            ? h('p', { class: 'muted small' }, 'Creating a catalog needs an admin key that reaches every catalog.')
            : null);
    mounted = { main, load };
    await load();
}

// The member-access section the catalog drawer shows (or last showed):
// {catalog, ticket, refresh, report}. See memberAccess.
let shownMemberAccess = null;

// The drawer's "Team member access": what members of the catalog's owner
// team reach through their accounts (the ceiling's level for it; keys and
// grants they create are capped at it). It is read from GET
// /catalogs/{catalog}/member-access when the drawer opens and again after
// every save, refused or not, so it always shows the server's value. A
// refusal's reason stays in the section until the next save. A read that
// fails keeps the last value, marked as possibly out of date, and leaves
// the controls usable, so the save can be tried again. Changing it
// needs `allowed` (an admin that reaches every catalog: the server's one
// check is `may_set_member_access`). `ticket` is the drawer's claim: an
// answer that arrives after the drawer was replaced or dismissed is
// dropped. Returns the section and `refresh()`, which the caller runs once
// the drawer is open.
//
// A save that settles updates the section the drawer shows now if it is
// this catalog's, through that section's own ticket, even when the drawer
// was closed and opened again while the save ran (`shownMemberAccess`); a
// drawer of another catalog is left alone. Each section draws only its
// newest read, so a read that started before the save never draws over
// the one after it.
function memberAccess(row, allowed, ticket) {
    const path = `/catalogs/${enc(row.name)}/member-access`;
    // The pending-write key (core/pending.js): a save in flight keeps these
    // controls disabled, and those of a drawer reopened meanwhile too.
    const key = `catalog:${row.name}:member-access`;
    // Whether a read has finished, the server's value as last read (null
    // until a read succeeds), and whether the latest read failed.
    let loaded = false;
    let value = null;
    let stale = false;
    const select = guard(h('select', { id: 'catalog-member-access-select', 'aria-label': 'Team member access' },
        ['none', 'read', 'write'].map(option => h('option', { value: option }, option))), key, () => allowed && loaded);
    const current = h('span', { id: 'catalog-member-access-current', class: 'mono' }, '...');
    // Save is offered when the choice differs from the server's value, or
    // always while that value is not known to be current.
    const save = guard(h('button', { type: 'button', id: 'catalog-member-access-save' }, 'Save'), key,
        () => allowed && loaded && (stale || select.value !== value));
    const refusal = h('p', { id: 'catalog-member-access-error', class: 'error small', role: 'alert', hidden: true });
    const sync = () => { recheck(select); recheck(save); };
    select.addEventListener('change', sync);

    let reads = 0;
    async function refresh() {
        const mine = ++reads;
        let answer = null;
        try { answer = await api.get(path); } catch { /* toasted */ }
        if (mine !== reads || !ticket.current()) return;
        loaded = true;
        stale = answer === null;
        if (!stale) {
            value = answer.member_access;
            select.value = value;
        }
        current.textContent = `${value ?? 'unknown'}${stale ? ' (could not be read again, may be out of date)' : ''}`;
        sync();
    }

    // Shows the reason a save was refused, or clears it (null).
    const report = reason => {
        refusal.textContent = reason ?? '';
        refusal.hidden = reason === null;
    };
    const self = { catalog: row.name, ticket, refresh, report };
    shownMemberAccess = self;

    save.addEventListener('click', () => once(key, async () => {
        let refused = null;
        try {
            const answer = await api.put(path, { access: select.value });
            toast(`Members of ${row.owner_team ?? 'the owner team'} now reach ${answer.name} at ${answer.member_access}.`, 'ok');
        } catch (error) {
            refused = error.message;   // already toasted
        }
        const shown = shownMemberAccess;
        const view = shown?.catalog === row.name && shown.ticket.current() ? shown : self;
        view.report(refused);
        await view.refresh();
    }));

    const section = h('section', { id: 'catalog-member-access' },
        h('h3', {}, 'Team member access'),
        h('p', { class: 'muted small' },
            `What members of ${row.owner_team ?? 'its owner team'} reach through their accounts: `
            + 'none, read or write. Their own keys and grants are capped at it.'),
        h('p', { class: 'small' }, 'Now: ', current),
        h('div', { class: 'inline-form' }, select, save),
        refusal,
        allowed ? null : h('p', { class: 'muted small' }, 'Changing it needs an admin key that reaches every catalog.'));
    return { section, refresh };
}

// The entitlements section the catalog drawer shows (or last showed):
// {catalog, ticket, refresh, report, take}. See entitlements.
let shownEntitlements = null;

// The owner drawer's "Entitlements": the users given this catalog beyond
// the team's member access, and a form to entitle another member of the
// owning team at read or write (an owner may entitle members only, never
// admin). It reads GET /catalogs/{catalog}/entitlements when the drawer
// opens and after every write, refused or not. Writes are pending writes
// keyed by the catalog (and by user for a removal) in core/pending.js, so a
// drawer reopened while one runs draws its controls disabled. A settled
// write draws its own answer in the section the drawer shows now if it is
// this catalog's (`shownEntitlements`), takes a read number before anything
// else awaits so an older read never draws over it, keeps a refusal's
// reason in #catalog-entitlements-error until the next write, and reads
// again; a read that fails keeps what was drawn, marked as possibly out of
// date, with the controls usable. Returns the section and `refresh()`.
function entitlements(row, ticket) {
    const name = row.name;
    const path = `/catalogs/${enc(name)}/entitlements`;
    const writeKey = `catalog:${name}:entitle`;
    const removeKey = user => `catalog:${name}:unentitle:${user}`;
    let loaded = false;
    let stale = false;
    let members = [];   // [{user, label, role}] the owning team's members drawn so far
    let given = [];     // [{user, label, access}] this catalog's entitlements drawn so far
    // The cursors that continue each list (null: no more), kept with the rows
    // they continue and replaced only when a first page draws.
    let membersNext = null;
    let givenNext = null;
    let continuing = false;   // a continuation read is pending

    const who = guard(h('select', { id: 'catalog-entitle-user', 'aria-label': 'Team member' }),
        writeKey, () => loaded && members.length > 0);
    const level = guard(h('select', { id: 'catalog-entitle-level', 'aria-label': 'Level' },
        ['read', 'write'].map(option => h('option', { value: option }, option))), writeKey, () => loaded);
    const add = guard(h('button', { type: 'button', id: 'catalog-entitle-save' }, 'Entitle'),
        writeKey, () => loaded && who.value !== '');
    who.addEventListener('change', () => recheck(add));
    const list = h('ul', { id: 'catalog-entitlements-list' });
    const note = h('p', { id: 'catalog-entitlements-stale', class: 'muted small', role: 'status', hidden: true },
        'Could not be read again; this may be out of date.');
    const refusal = h('p', { id: 'catalog-entitlements-error', class: 'error small', role: 'alert', hidden: true });
    const moreMembers = h('button', { type: 'button', id: 'catalog-members-more', class: 'secondary small', hidden: true,
                                      onclick: () => more('members') }, 'Load more members');
    const moreGiven = h('button', { type: 'button', id: 'catalog-entitlements-more', class: 'secondary small', hidden: true,
                                    onclick: () => more('entitlements') }, 'Load more entitlements');

    const labelOf = user => members.find(member => member.user === user)?.label ?? user;
    const optionText = member => (member.label === member.user ? member.user : `${member.label} (${member.user})`);

    function draw() {
        const keep = who.value;
        who.replaceChildren(
            h('option', { value: '', disabled: true, selected: true }, 'Choose a member...'),
            ...members.map(member => h('option', { value: member.user }, optionText(member))));
        if (members.some(member => member.user === keep)) who.value = keep;
        list.replaceChildren(...(given.length
            ? given.map(entry => h('li', { dataset: { user: entry.user } },
                `${entry.label}: ${entry.access} `,
                guard(h('button', { type: 'button', class: 'secondary small', dataset: { remove: entry.user },
                                    onclick: () => unentitle(entry.user) }, 'Remove'),
                    removeKey(entry.user), () => loaded)))
            : [h('li', { class: 'muted' }, loaded ? 'No one is entitled beyond the team\'s member access.' : '...')]));
        note.hidden = !stale;
        // A continuation shows only while the rows drawn have a cursor and no
        // continuation is pending (a second click would append twice).
        moreMembers.hidden = membersNext === null || continuing;
        moreGiven.hidden = givenNext === null || continuing;
        recheck(who); recheck(level); recheck(add);
    }

    let reads = 0;
    async function refresh() {
        const mine = ++reads;
        let answer = null;
        try { answer = await api.get(path); } catch { /* toasted */ }
        if (mine !== reads || !ticket.current()) return;
        loaded = true;
        stale = answer === null;
        if (answer !== null) {
            members = answer.members;
            given = answer.entitlements;
            membersNext = answer.members_next ?? null;
            givenNext = answer.entitlements_next ?? null;
        }
        continuing = false;
        draw();
    }

    // The page after the rows drawn of one list, appended; dropped when a
    // newer read has started since (it replaces the rows and cursors). A
    // failure keeps the cursor for a retry and marks the rows possibly out
    // of date.
    async function more(which) {
        const cursor = which === 'members' ? membersNext : givenNext;
        if (cursor === null || continuing) return;
        const mine = reads;
        continuing = true;
        draw();
        const query = new URLSearchParams({ list: which, [`${which === 'members' ? 'members' : 'entitlements'}_after`]: cursor });
        let answer = null;
        try { answer = await api.get(`${path}?${query}`); } catch { /* toasted */ }
        if (mine !== reads || !ticket.current()) return;
        continuing = false;
        if (answer === null) {
            stale = true;
        } else if (which === 'members') {
            members = members.concat(answer.members);
            membersNext = answer.members_next ?? null;
        } else {
            given = given.concat(answer.entitlements);
            givenNext = answer.entitlements_next ?? null;
        }
        draw();
    }

    // Draws a settled write's answer (`apply` changes the entitlements, or
    // null for a refusal) and cancels any read that started before it.
    const take = apply => {
        reads++;
        continuing = false;
        if (apply !== null) {
            given = apply(given);
            stale = false;
        }
        loaded = true;
        draw();
    };
    const report = reason => {
        refusal.textContent = reason ?? '';
        refusal.hidden = reason === null;
    };
    const self = { catalog: name, ticket, refresh, report, take };
    shownEntitlements = self;

    // Runs one write, then draws it in the section the drawer shows now.
    function write(key, send, apply, done) {
        return once(key, async () => {
            let change = null;
            let refused = null;
            try {
                await send();
                toast(done, 'ok');
                change = apply;
            } catch (error) {
                refused = error.message;   // already toasted
            }
            const shown = shownEntitlements;
            const view = shown?.catalog === name && shown.ticket.current() ? shown : self;
            view.report(refused);
            view.take(change);
            await view.refresh();
        });
    }

    function unentitle(user) {
        return write(removeKey(user),
            () => api.del(`/users/${enc(user)}/entitlements/${enc(name)}`),
            entries => entries.filter(entry => entry.user !== user),
            `${labelOf(user)} no longer has ${row.label} beyond the team's access.`);
    }

    add.addEventListener('click', () => {
        const user = who.value;
        const access = level.value;
        if (!user) return;
        write(writeKey,
            () => api.put(`/users/${enc(user)}/entitlements/${enc(name)}`, { access }),
            entries => entries.filter(entry => entry.user !== user)
                .concat([{ user, label: labelOf(user), access }])
                .sort((a, b) => (a.user < b.user ? -1 : 1)),
            `${labelOf(user)} now reaches ${row.label} at ${access}.`);
    });

    const section = h('section', { id: 'catalog-entitlements' },
        h('h3', {}, 'Entitlements'),
        h('p', { class: 'muted small' },
            `Raise a member of ${row.owner_team_label ?? row.owner_team ?? 'the owner team'} above the team's member access, `
            + 'at read or write. Only members of the team can be entitled.'),
        list,
        moreGiven,
        h('div', { class: 'inline-form' }, who, level, add),
        moreMembers,
        note,
        refusal);
    draw();
    return { section, refresh };
}

// A catalog drawer's subtitle.
// The drawer's Store section: the store and where it is. Null when the
// answer carries no `store` (a key that may not see stores).
function storeSection(row) {
    if (!row.store) return null;
    return h('section', { id: 'catalog-store', dataset: { test: 'catalog-store' } },
        h('h3', {}, 'Store'),
        h('p', {}, h('span', { class: 'mono', id: 'catalog-store-name' }, row.store.name), ' ',
            h('span', { class: 'muted small', id: 'catalog-store-location' }, storeLocation(row.store))),
        h('p', { class: 'muted small' }, 'A catalog stays on the store it was created on.'));
}

function subtitle(row) {
    return `generation ${row.generation} - ${count(row.concepts, 'concept')}, ${count(row.files, 'file')}`;
}

// Whether to enable `row`'s maintenance switch: for an every-catalog admin,
// and for an owner of the catalog's team (GET /me's `owned_teams`, a
// console session's). Only a hint, since the server checks the owner under
// its locks: when /me cannot be read, the switch is offered and the server
// decides.
async function maySwitchMaintenance(row) {
    if (canAdministerPeople()) return true;
    if (!store.session || !row.owner_team) return false;
    try {
        return (await freshAccount()).owned_teams.includes(row.owner_team);
    } catch {
        return true;   // toasted; the server refuses a non-owner
    }
}

// Whether to offer `row`'s member access and entitlements to an owner: a
// console session that owns the catalog's team (GET /me's
// `owned_team_details`) when that team is shared, never a personal one.
// Only a hint, since the server checks the owner under its locks; when
// /me cannot be read nothing is offered.
async function maySetTeamAccess(row) {
    if (!store.session || !row.owner_team) return false;
    try {
        const team = (await freshAccount()).owned_team_details.find(owned => owned.name === row.owner_team);
        return Boolean(team) && !team.personal;
    } catch {
        return false;   // toasted
    }
}

// Catalog `name` as GET /catalogs lists it now, or null when no page lists
// it. The listing answers a page at a time (the default size, as the table
// reads it), so the pages are read in turn until one holds it. Rejects as api.get does (toasted).
async function listedCatalog(name) {
    let cursor = null;
    do {
        const page = await api.get(cursor === null ? '/catalogs' : `/catalogs?${new URLSearchParams({ after: cursor })}`);
        const found = page.catalogs.find(catalog => catalog.name === name);
        if (found) return found;
        // A cursor that does not move would never end: stop at it.
        const next = page.next ?? null;
        cursor = next === cursor ? null : next;
    } while (cursor !== null);
    return null;
}

// The maintenance section the catalog drawer shows (or last showed):
// {catalog, ticket, refresh, report, take}. See maintenance.
let shownMaintenance = null;

// The drawer's "Maintenance" section: the switch that lets the maintenance
// runner check this catalog (`on`) or keeps it out (`off`). It is read
// again from GET /catalogs when the drawer opens (the caller runs
// `refresh()`), and the switch stays disabled until that read settles: the
// table's row may be older than a write that settled since (the table is
// not reloaded after one). Toggling it is a pending write keyed
// `catalog:<name>:maintenance` (core/pending.js), so a drawer opened again
// while it runs draws the switch disabled. A settled write draws its answer
// in the section the drawer shows now if it is this catalog's (through
// `shownMaintenance`, as memberAccess does), keeps a refusal's reason in
// #catalog-maintenance-error until the next write, and reads the catalogs
// again (GET /catalogs). Reads are numbered, and a write takes the next
// number as it settles, so an older read never draws over it. A read that
// fails keeps the last value, marked as possibly out of date, with the
// switch usable. Returns the section and `refresh()`.
function maintenance(row, allowed, ticket) {
    const name = row.name;
    const key = `catalog:${name}:maintenance`;
    let value = row.maintenance;
    let stale = false;
    // Whether a read or a write has answered since the drawer opened.
    let loaded = false;
    const toggle = guard(h('input', { type: 'checkbox', role: 'switch', id: 'catalog-maintenance',
                                      'aria-label': `Maintenance for ${name}` }), key, () => allowed && loaded);
    const state = h('span', { id: 'catalog-maintenance-state', class: 'mono' });
    const refusal = h('p', { id: 'catalog-maintenance-error', class: 'error small', role: 'alert', hidden: true });
    const draw = () => {
        toggle.checked = value === 'on';
        state.textContent = loaded
            ? `${value}${stale ? ' (could not be read again, may be out of date)' : ''}`
            : '...';
        recheck(toggle);
    };
    // Draws a server answer (null: the last value again).
    const settle = answer => {
        loaded = true;
        if (answer !== null) value = answer;
        draw();
    };
    draw();

    let reads = 0;
    async function refresh() {
        const mine = ++reads;
        let found = null;
        try { found = await listedCatalog(name); } catch { /* toasted */ }
        if (mine !== reads || !ticket.current()) return;
        stale = !found;
        settle(found?.maintenance ?? null);
    }
    // Draws a settled write's answer (null for a refusal: the last value
    // again, so the switch shows the server's state) and cancels any read
    // that started before it.
    const take = answer => {
        reads++;
        if (answer !== null) stale = false;
        settle(answer);
    };
    const report = reason => {
        refusal.textContent = reason ?? '';
        refusal.hidden = reason === null;
    };
    const self = { catalog: name, ticket, refresh, report, take };
    shownMaintenance = self;

    toggle.addEventListener('change', () => {
        const to = toggle.checked ? 'on' : 'off';
        once(key, async () => {
            let answer = null;
            let refused = null;
            try {
                answer = (await api.put(`/catalogs/${enc(name)}/maintenance`, { maintenance: to })).maintenance;
                toast(`Maintenance is ${answer} for ${row.label}.`, 'ok');
            } catch (error) {
                refused = error.message;   // already toasted
            }
            const shown = shownMaintenance;
            const view = shown?.catalog === name && shown.ticket.current() ? shown : self;
            view.report(refused);
            view.take(answer);
            await view.refresh();
        });
    });

    const section = h('section', { id: 'catalog-maintenance-section' },
        h('h3', {}, 'Maintenance'),
        h('label', {}, toggle, ' Maintenance runner: ', state),
        h('p', { class: 'muted small' },
            'Lets the maintenance runner check this catalog for stale, bloated and broken notes. Turn it off to keep the runner out.'),
        refusal,
        allowed ? null : h('p', { class: 'muted small' },
            `Changing it needs an admin that reaches every catalog, or an owner of ${row.owner_team ?? 'its owner team'}.`));
    return { section, refresh };
}
