// Unlocks (M14 Task 9e): who unlocked which customer catalog, why, and for
// how long, newest first, with what each unlock was used for (reads, writes
// and exports). An admin reads full records (a scoped admin only those of
// the catalogs it administers), unlocks a catalog for itself and ends a
// live unlock early. A person signed in with an account who is not an admin
// picks one of the teams they own and reads that team's catalogs' unlocks
// as the server redacts them (plan Decisions 14 and 18): no reason and no
// staff member, by "an operator", and nothing to end.
//
// Every table load (Refresh, the team picker, the reload after an End or an
// unlock) is numbered, and only the newest one draws. End is a pending write
// keyed `unlock:<id>:end` (core/pending.js), so a table drawn again while it
// runs draws that End disabled. A settled End puts its answer into the rows
// before the reload, a refused one keeps its reason beside the row, and a
// reload that fails draws the last rows again, marked as possibly out of
// date, with every control usable.
import { h, fill, pill } from '../core/dom.js';
import { api, enc } from '../core/api.js';
import { store } from '../core/store.js';
import { catalogLabel, ensureLabels, named, userLabel } from '../core/labels.js';
import { toast } from '../core/toast.js';
import { when } from '../core/format.js';
import { listTable } from '../components/list-table.js';
import { confirmDialog } from '../components/confirm.js';
import { CATEGORY_LABEL, canUnlock, openUnlock } from '../components/unlock.js';
import { freshAccount, refreshIdentity } from '../core/shell.js';
import { guard, once } from '../core/pending.js';

const STATE_KIND = { live: 'warn', expired: 'muted', ended: 'muted' };
const NO_TEAM = "You own no team; a team's owners read the unlocks of its catalogs here.";
const DENIED = 'Unlocks are for an admin, or for a team owner signed in with their account.';

// A time in the browser's local time ("-" for none).
const at = iso => h('span', {}, when(iso));

export async function render(main) {
    const admin = store.whoami?.access === 'admin';
    const title = h('h1', {}, 'Unlocks');
    if (!admin && !store.session) {
        fill(main, title, h('p', { class: 'muted', id: 'unlocks-denied' }, DENIED));
        return;
    }
    // A team owner reads one owned team at a time (`?team=`).
    let owned = [];
    if (!admin) {
        try {
            owned = (await freshAccount()).owned_teams;
        } catch {
            if (main.isConnected) {
                fill(main, title, h('p', { class: 'error', id: 'unlocks-error' },
                    'Your teams could not be read. Reload the page to try again.'));
            }
            return;   // toasted
        }
        if (!main.isConnected) return;
        if (!owned.length) {
            fill(main, title, h('p', { class: 'muted', id: 'unlocks-no-team' }, NO_TEAM));
            return;
        }
    }
    const team = admin ? null
        : h('select', { id: 'unlocks-team', 'aria-label': 'Team' }, owned.map(name => h('option', { value: name }, name)));

    // The reason each End was refused, by unlock id, drawn beside its row
    // until that End's next try succeeds.
    const refusals = new Map();
    const endKey = row => `unlock:${row.id}:end`;

    async function end(row) {
        const ok = await confirmDialog({
            title: `End the unlock of ${catalogLabel(row.catalog)}?`,
            message: `${userLabel(row.user)} loses operator access to ${catalogLabel(row.catalog)} now.`,
            action: 'End',
        });
        if (!ok) return;
        await once(endKey(row), async () => {
            try {
                const ended = await api.del(`/unlocks/${enc(row.id)}`);
                refusals.delete(row.id);
                // The answer is drawn even when the reload after it fails.
                rows = rows.map(shown => (shown.id === ended.id ? ended : shown));
                toast(`Ended the unlock of ${catalogLabel(ended.catalog)}.`, 'ok');
                // The catalog may no longer be readable: the switcher drops it.
                await refreshIdentity().catch(() => {});
            } catch (error) {
                refusals.set(row.id, error.message);   // already toasted
            }
            await load();
        });
    }

    const actions = row => [
        row.state === 'live'
            ? guard(h('button', { type: 'button', class: 'secondary', dataset: { test: `unlock-end-${row.catalog}` },
                                  onclick: () => end(row) }, 'End'), endKey(row))
            : null,
        refusals.has(row.id)
            ? h('p', { class: 'error small', role: 'alert', dataset: { test: `unlock-error-${row.id}` } }, refusals.get(row.id))
            : null,
    ];
    const columns = [
        { key: 'catalog', label: 'Catalog', sort: row => row.catalog, render: row => named(catalogLabel(row.catalog), row.catalog) },
        admin ? { key: 'user', label: 'User', sort: row => row.user, render: row => named(userLabel(row.user), row.user) } : null,
        admin ? null : { key: 'by', label: 'By', render: row => h('span', { class: 'muted' }, row.by) },
        { key: 'category', label: 'Category', sort: row => row.category,
          render: row => CATEGORY_LABEL[row.category] ?? row.category },
        admin ? { key: 'reason', label: 'Reason', render: row => row.reason } : null,
        { key: 'starts', label: 'From', sort: row => row.starts, render: row => at(row.starts) },
        { key: 'until', label: 'Until', sort: row => until(row), render: row => at(until(row)) },
        { key: 'state', label: 'State', sort: row => row.state, render: row => pill(row.state, STATE_KIND[row.state]) },
        { key: 'reads', label: 'Reads', sort: row => row.reads, class: 'num' },
        { key: 'writes', label: 'Writes', sort: row => row.writes, class: 'num' },
        { key: 'exports', label: 'Exports', sort: row => row.exports, class: 'num' },
        admin ? { key: 'actions', label: '', render: actions } : null,
    ].filter(Boolean);
    // An admin's row ends at `ended` or else `expires`; an owner's at `ends`.
    const until = row => (admin ? row.ended ?? row.expires : row.ends);

    let rows = [];
    let loads = 0;   // the number of the newest table load
    const table = listTable(columns, rows, { empty: 'No unlocks.' });
    table.id = 'unlocks-table';
    const stale = h('p', { id: 'unlocks-stale', class: 'error small', role: 'status', hidden: true },
        'Could not read the unlocks again, so what is shown may be out of date.');

    // Reads the unlocks and draws them; a load a newer one has overtaken
    // draws nothing, and a failed one (toasted) draws the last rows again,
    // marked stale.
    async function load() {
        const mine = ++loads;
        let answer = null;
        try {
            answer = await api.get(admin ? '/unlocks' : `/unlocks?${new URLSearchParams({ team: team.value })}`);
        } catch { /* toasted */ }
        // An admin's rows name their users: read those labels (a failed
        // read shows the slugs; it never stops the rows from drawing).
        if (admin && answer !== null) await ensureLabels({ users: answer.unlocks.map(row => row.user) });
        if (mine !== loads || !main.isConnected) return;
        stale.hidden = answer !== null;
        if (answer !== null) rows = answer.unlocks;
        table.redraw(rows);
    }
    if (team) team.addEventListener('change', () => { rows = []; load(); });

    fill(main,
        h('header', { class: 'page-head' },
            h('div', {}, title,
                h('p', { class: 'muted' }, admin
                    ? 'Operator access to customers\' catalogs: who, why, for how long, and what it was used for.'
                    : 'When operator staff unlocked your team\'s catalogs, why in general, and what they did. Who it was is not shown.')),
            h('div', { class: 'actions' },
                team,
                h('button', { type: 'button', id: 'unlocks-refresh', class: 'secondary', onclick: () => load() }, 'Refresh'),
                admin && canUnlock()
                    ? h('button', { type: 'button', id: 'unlocks-new',
                                    onclick: () => openUnlock(null, { view: main, onDone: () => { if (main.isConnected) load(); } }) },
                        'Unlock a catalog')
                    : null)),
        stale,
        table);
    await load();
}
