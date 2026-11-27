// The reader's routes, in the URL hash, every segment percent-encoded:
//   #/                                    home
//   #/c[?gone=<catalog>]                  catalog picker
//   #/t/<catalog>[/<folder>...]           a folder of the tree
//   #/s/<catalog>[?q=<query>]             search
//   #/v/<catalog>/<path>.md[?s=<section>] a concept, optionally at a heading
//   #/d/<catalog>/<path>                  a log.md or AGENTS.md (read as a document)
//   #/me                                  account
// A concept's path is every segment after the catalog (a folder may be
// named 'x.md'), so its section travels as a parameter, never a segment.
const dec = decodeURIComponent;
const enc = encodeURIComponent;

function segments(path) {
    return String(path ?? '').replace(/^\/+/, '').split('/').filter(Boolean);
}

export function parse(hash) {
    const [body, query = ''] = hash.replace(/^#/, '').split('?');
    const [screen = '', catalog = null, ...rest] = body.split('/').filter(Boolean).map(dec);
    const params = new URLSearchParams(query);
    switch (screen) {
        case 'c': return { screen: 'catalogs', gone: params.get('gone') };
        case 't': return { screen: 'tree', catalog, folder: rest.join('/') };
        case 's': return { screen: 'search', catalog, query: params.get('q') ?? '' };
        case 'v': {
            const path = rest.join('/');
            if (!path.endsWith('.md')) return { screen: 'tree', catalog, folder: path };
            return { screen: 'concept', catalog, path, section: params.get('s') || null };
        }
        case 'd': return { screen: 'document', catalog, path: rest.join('/') };
        case 'me': return { screen: 'me' };
        default: return { screen: 'home' };
    }
}

export const hrefs = Object.freeze({
    home: () => '#/',
    catalogs: (gone = null) => (gone ? `#/c?gone=${enc(gone)}` : '#/c'),
    tree: (catalog, folder = '') => `#/t/${[catalog, ...segments(folder)].map(enc).join('/')}`,
    search: (catalog, query = '') => `#/s/${enc(catalog)}${query ? `?q=${enc(query)}` : ''}`,
    concept: (catalog, path, section = null) =>
        `#/v/${[catalog, ...segments(path)].map(enc).join('/')}${section ? `?s=${enc(section)}` : ''}`,
    document: (catalog, path) => `#/d/${[catalog, ...segments(path)].map(enc).join('/')}`,
    me: () => '#/me',
});

// The folder holding `path` ('a/b/c.md' -> 'a/b').
export function parentOf(path) {
    return segments(path).slice(0, -1).join('/');
}

// The files the server serves through /documents, not /concepts (as the
// console's browse.js).
export const DOCUMENTS = new Set(['index.md', 'log.md', 'AGENTS.md']);

// Where a document opens: a folder's index.md opens its folder, a log.md or
// AGENTS.md the document view, anything else the concept view. Every link
// the reader makes (recent changes, backlinks, search hits, body links,
// the offline list) goes through here.
export function hrefFor(catalog, path, section = null) {
    const parts = segments(path);
    const name = parts[parts.length - 1];
    if (name === 'index.md') return hrefs.tree(catalog, parts.slice(0, -1).join('/'));
    if (DOCUMENTS.has(name)) return hrefs.document(catalog, parts.join('/'));
    return hrefs.concept(catalog, parts.join('/'), section);
}

// A link src/render.rs wrote ('#/browse/<catalog>/<path>[?s=<section>]', the
// console's route; links written before the parameter ended in
// '/<section>') as the reader's route, or null for any other link.
// render.rs links only to a path ending in '.md', plus at most one section
// segment in the old form, so the document ends at the LAST segment ending in '.md' (a
// folder before it may be named 'x.md'), and what follows is the section.
// A heading's slug (src/okf/markdown.rs, `slugify`) keeps only letters,
// digits, '_' and '-', so a section that reaches a heading never ends in
// '.md'. Only a link whose #fragment ends in '.md', which reaches no
// heading anyway, is read as a longer path.
export function fromBrowse(href) {
    if (!href.startsWith('#/browse/')) return null;
    const [path, query = ''] = href.slice('#/browse/'.length).split('?');
    const [catalog, ...rest] = path.split('/').map(dec);
    const end = rest.findLastIndex(segment => segment.endsWith('.md'));
    if (end < 0) return hrefs.tree(catalog, rest.join('/'));
    const section = new URLSearchParams(query).get('s') || rest.slice(end + 1).join('/') || null;
    return hrefFor(catalog, rest.slice(0, end + 1).join('/'), section);
}
