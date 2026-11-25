// Recent changes: the catalog's Health (components/health.js), then the
// revisions published in the selected catalog, every
// document's, newest first (#/recent). A row opens that revision's diff and
// text in the drawer, a deleted document's too. Refresh reads the newest
// page again; Load older continues from the last row.
import { h, pill } from '../core/dom.js';
import { api, enc } from '../core/api.js';
import { store } from '../core/store.js';
import { catalogLabel, named } from '../core/labels.js';
import { closeDrawer } from '../core/drawer.js';
import { ago } from '../core/format.js';
import { listTable } from '../components/list-table.js';
import { openRevision } from '../components/revision.js';
import { renderHealth } from '../components/health.js';

const OP_KIND = { create: 'ok', update: 'info', delete: 'error', import: 'muted' };
let pageSize = 50;

// A test hook: a journey shrinks the page so a few revisions make several pages.
export function setRecentPageSize(size) {
    pageSize = size;
}

// One page, newest first; `after` (the previous page's `next_after`)
// continues past its last row.
function recentPage(catalog, after) {
    const query = new URLSearchParams({ limit: String(pageSize) });
    if (after) query.set('after', after);
    return api.get(`/catalogs/${enc(catalog)}/recent-changes?${query}`);
}

function columns() {
    return [
        { key: 'at', label: 'When', render: row => ago(row.at), sort: row => row.at },
        { key: 'path', label: 'Document', class: 'mono', sort: row => row.path },
        { key: 'revision', label: 'Rev', sort: row => row.revision },
        { key: 'op', label: 'Op', render: row => pill(row.op, OP_KIND[row.op] ?? 'muted'), sort: row => row.op },
        // The actor's label, with its id beneath when the directory knows one.
        { key: 'actor', label: 'Actor', render: row => (row.actor_label ? named(row.actor_label, row.actor) : row.actor), sort: row => row.actor },
        { key: 'message', label: 'Message' },
    ];
}

export async function render(main) {
    // Read once: a catalog switch routes again, into a new `main`, so this
    // render only ever shows `catalog`.
    const catalog = store.catalog;
    // A drawer left open by the previous page or catalog would describe
    // something this page does not show.
    closeDrawer();
    const refreshButton = h('button', { type: 'button', id: 'recent-refresh', class: 'secondary', onclick: () => refresh() }, 'Refresh');
    main.append(h('header', { class: 'page-head' },
        h('div', {}, h('h1', {}, 'Recent changes'),
            h('p', { class: 'muted', id: 'recent-scope' },
                catalog ? `Published changes in ${catalogLabel(catalog)}, newest first.` : 'No catalog selected.')),
        catalog ? h('div', { class: 'actions' }, refreshButton) : null));
    if (!catalog) return;
    // Health first; its reads and the revision list's fail independently.
    const health = h('section', { id: 'health' });
    const healthLoaded = renderHealth(health, catalog);
    const status = h('p', { class: 'muted', id: 'recent-status', role: 'status' }, 'Loading...');
    const table = listTable(columns(), [], {
        onRow: row => openRevision(main, catalog, row.path, row),
        empty: 'No changes have been published in this catalog yet.',
    });
    table.id = 'recent-table';
    table.hidden = true;
    const older = h('button', { type: 'button', id: 'recent-older', class: 'secondary', hidden: true, onclick: () => loadOlder() }, 'Load older');
    main.append(health, h('h2', { class: 'history-head' }, 'Revision history'), status, table, older);

    // The rows drawn and the cursor that continues them are one committed
    // state: a first page replaces both, a continuation appends to both,
    // and a failed or overtaken read changes neither, so Load older
    // survives it for a retry. Every read takes a number; an answer whose
    // number a newer read has passed, or that arrives after the page was
    // left (a navigation or a catalog switch detaches `main`), is dropped.
    let rows = [];
    let next = null;
    let loaded = false;
    let ticket = 0;
    let pending = false;

    const showStatus = text => {
        status.textContent = text ?? '';
        status.hidden = !text;
    };
    const settle = () => {
        pending = false;
        older.hidden = !next;
    };
    const failed = (error, what) => {
        const reason = error?.message ? ` ${error.message}` : '';
        showStatus(loaded
            ? `${what} failed; the list below may be out of date.${reason}`
            : `Recent changes could not be loaded.${reason} Try Refresh.`);
        status.classList.toggle('error', true);
    };

    async function refresh() {
        const mine = ++ticket;
        pending = true;
        older.hidden = true;
        if (!loaded) showStatus('Loading...');
        let page;
        try {
            page = await recentPage(catalog, null);
        } catch (error) {
            if (mine !== ticket || !main.isConnected) return;
            failed(error, 'Refresh');
            settle();
            return;
        }
        if (mine !== ticket || !main.isConnected) return;
        rows = page.changes;
        next = page.next_after ?? null;
        loaded = true;
        status.classList.toggle('error', false);
        showStatus(null);
        table.hidden = false;
        table.redraw(rows);
        settle();
    }

    async function loadOlder() {
        if (pending || !next) return;
        const mine = ++ticket;
        pending = true;
        older.hidden = true;
        let page;
        try {
            page = await recentPage(catalog, next);
        } catch (error) {
            if (mine !== ticket || !main.isConnected) return;
            failed(error, 'Loading older changes');
            settle();
            return;
        }
        if (mine !== ticket || !main.isConnected) return;
        rows = rows.concat(page.changes);
        next = page.next_after ?? null;
        status.classList.toggle('error', false);
        showStatus(null);
        table.redraw(rows);
        settle();
    }

    await Promise.all([healthLoaded, refresh()]);
}
