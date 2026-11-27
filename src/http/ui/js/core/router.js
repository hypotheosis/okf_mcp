// Hash routing: '#/<page>/<rest...>' loads js/pages/<page>.js and calls its
// render(view, rest). /ui/manifest.json says which pages this build has, so
// a nav link to a page not built yet is hidden, never a 404.
//
// Each navigation renders into its own fresh container (`view`), and the
// previous one is detached from #main. A render still awaiting an API answer
// when the user moves on therefore writes into a detached element, never
// into the newer page. Pages check `view.isConnected` after an await before
// touching anything outside `view` (the drawer, the nav badge).
import { h } from './dom.js';

let pages = new Set();
let started = false;
const DEFAULTS = ['browse', 'keys'];

// '#/<page>/<segments...>[?<params>]': the segments are decoded, and the
// parameters (a raw '?' never occurs inside an encoded segment) come back
// as a URLSearchParams.
export function parse(hash) {
    const [path, query = ''] = hash.replace(/^#\/?/, '').split('?');
    const parts = path.split('/').filter(Boolean).map(decodeURIComponent);
    return { page: parts.shift() ?? '', rest: parts, query: new URLSearchParams(query) };
}

// '#/browse/<catalog>/<path segments>[?s=<section slug>]', each segment and
// the slug encoded (parse() decodes them). src/render.rs (`browse_route`)
// writes the same shape into rendered links. The section is a parameter,
// never a segment, so a folder named 'x.md' is not mistaken for a document
// with a section. path may start with '/' or not.
export function browseHash(catalog, path = '', section = null) {
    const parts = [catalog, ...String(path).replace(/^\/+/, '').split('/').filter(Boolean)];
    const hash = '#/browse/' + parts.map(encodeURIComponent).join('/');
    return section ? `${hash}?s=${encodeURIComponent(section)}` : hash;
}

// '#/users/detail/<name>': the Users page with that user's drawer open. A
// route of its own, so no name ("unassigned") is taken for another view;
// the name is one encoded segment.
export function userHash(name) {
    return `#/users/detail/${encodeURIComponent(name)}`;
}

export function go(hash) {
    if (location.hash === hash) route();
    else location.hash = hash;
}

// Replaces #main's content with a fresh view and runs render(view, rest) in it.
// Exported for the router journey in tests/ui_e2e/login.rs.
export async function mount(render, rest, query = new URLSearchParams()) {
    const view = h('div', { class: 'view' });
    document.getElementById('main').replaceChildren(view);
    await render(view, rest, query);
}

export async function route() {
    let { page, rest, query } = parse(location.hash);
    // The standalone Health page is part of Recent changes now: old links
    // land there, and the address bar says so.
    if (page === 'health') {
        history.replaceState(null, '', '#/recent');
        page = 'recent';
        rest = [];
    }
    if (!page) page = DEFAULTS.find(name => pages.has(name)) ?? '';
    document.querySelectorAll('.nav-link').forEach(link => link.classList.toggle('active', link.dataset.nav === page));
    if (!pages.has(page)) {
        await mount(view => view.append(h('h1', {}, page ? 'Not available yet' : 'Welcome'),
            h('p', { class: 'muted' }, page ? `This build has no ${page} page.` : 'No pages are installed in this build yet.')), rest);
        return;
    }
    const placeholder = location.hash;
    const module = await import(`../pages/${page}.js`);
    if (location.hash !== placeholder) return;   // the user moved on while it loaded
    await mount(module.render, rest, query);
}

export async function start() {
    const manifest = await (await fetch('/ui/manifest.json')).json();
    pages = new Set(manifest.pages);
    document.querySelectorAll('.nav-link').forEach(link => {
        // A link with no `data-nav` is external (Billing): not a page.
        if (link.dataset.nav && !pages.has(link.dataset.nav)) link.hidden = true;
    });
    if (!started) {
        addEventListener('hashchange', route);
        started = true;
    }
    await route();
}
