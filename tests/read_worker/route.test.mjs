// Tests of the reader's hash routes (src/http/read/js/route.js) and its
// folder listing (js/listing.js), run with `node --test tests/read_worker/`
// (`just test-worker`). Both are plain modules with no browser globals.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { page } from './page.mjs';

const { DOCUMENTS, fromBrowse, hrefFor, hrefs, parentOf, parse } = await page('route.js');
const { folderListing, folderTitles } = await page('listing.js');

test('parse reads every screen', () => {
    assert.deepEqual(parse(''), { screen: 'home' });
    assert.deepEqual(parse('#/'), { screen: 'home' });
    assert.deepEqual(parse('#/nowhere'), { screen: 'home' });
    assert.deepEqual(parse('#/c'), { screen: 'catalogs', gone: null });
    assert.deepEqual(parse('#/c?gone=team%20space'), { screen: 'catalogs', gone: 'team space' });
    assert.deepEqual(parse('#/t/main'), { screen: 'tree', catalog: 'main', folder: '' });
    assert.deepEqual(parse('#/t/main/a/b%20c'), { screen: 'tree', catalog: 'main', folder: 'a/b c' });
    assert.deepEqual(parse('#/s/main'), { screen: 'search', catalog: 'main', query: '' });
    assert.deepEqual(parse('#/s/main?q=a%26b%3Fc'), { screen: 'search', catalog: 'main', query: 'a&b?c' });
    assert.deepEqual(parse('#/v/main/a/b.md'), { screen: 'concept', catalog: 'main', path: 'a/b.md', section: null });
    assert.deepEqual(parse('#/v/main/a/b.md?s=caf%C3%A9'), { screen: 'concept', catalog: 'main', path: 'a/b.md', section: 'café' });
    assert.deepEqual(parse('#/v/main/a/b.md?s='), { screen: 'concept', catalog: 'main', path: 'a/b.md', section: null });
    assert.deepEqual(parse('#/d/main/a/log.md'), { screen: 'document', catalog: 'main', path: 'a/log.md' });
    assert.deepEqual(parse('#/me'), { screen: 'me' });
});

test('a concept route not ending in .md reads as its folder', () => {
    assert.deepEqual(parse('#/v/main/a/b'), { screen: 'tree', catalog: 'main', folder: 'a/b' });
    assert.deepEqual(parse('#/v/main/a.md/b'), { screen: 'tree', catalog: 'main', folder: 'a.md/b' });
});

test('a concept route\'s path is every segment, folders named x.md included', () => {
    assert.deepEqual(parse('#/v/main/archive.md/topic.md'),
        { screen: 'concept', catalog: 'main', path: 'archive.md/topic.md', section: null });
    assert.deepEqual(parse('#/v/main/archive.md/topic.md?s=notes'),
        { screen: 'concept', catalog: 'main', path: 'archive.md/topic.md', section: 'notes' });
    for (const [path, section] of [['archive.md/topic.md', null], ['archive.md/topic.md', 'notes'],
        ['a.md/b.md/c.md', 'x.md'], ['archive.md/topic.md', 'a/b?c=d&e']]) {
        assert.deepEqual(parse(hrefs.concept('main', path, section)),
            { screen: 'concept', catalog: 'main', path, section });
        assert.deepEqual(parse(hrefFor('main', path, section)),
            { screen: 'concept', catalog: 'main', path, section });
    }
});

test('every href parses back to the route it names, whatever the characters', () => {
    const catalog = 'team space/#?';
    const path = '/notes & more/café #1?.md';
    assert.deepEqual(parse(hrefs.home()), { screen: 'home' });
    assert.deepEqual(parse(hrefs.catalogs()), { screen: 'catalogs', gone: null });
    assert.deepEqual(parse(hrefs.catalogs(catalog)), { screen: 'catalogs', gone: catalog });
    assert.deepEqual(parse(hrefs.tree(catalog)), { screen: 'tree', catalog, folder: '' });
    assert.deepEqual(parse(hrefs.tree(catalog, '/a b/c/')), { screen: 'tree', catalog, folder: 'a b/c' });
    assert.deepEqual(parse(hrefs.search(catalog)), { screen: 'search', catalog, query: '' });
    assert.deepEqual(parse(hrefs.search(catalog, 'x & y?')), { screen: 'search', catalog, query: 'x & y?' });
    assert.deepEqual(parse(hrefs.concept(catalog, path)),
        { screen: 'concept', catalog, path: 'notes & more/café #1?.md', section: null });
    assert.deepEqual(parse(hrefs.concept(catalog, path, 'a/b')),
        { screen: 'concept', catalog, path: 'notes & more/café #1?.md', section: 'a/b' });
    assert.deepEqual(parse(hrefs.document(catalog, '/a/log.md')), { screen: 'document', catalog, path: 'a/log.md' });
    assert.deepEqual(parse(hrefs.me()), { screen: 'me' });
});

test('hrefs encode every segment and drop empty ones', () => {
    assert.equal(hrefs.tree('main', '/a//b/'), '#/t/main/a/b');
    assert.equal(hrefs.concept('main', '/a b/c.md', 'x y'), '#/v/main/a%20b/c.md?s=x%20y');
    assert.equal(hrefs.concept('main', '/a b/c.md'), '#/v/main/a%20b/c.md');
    assert.equal(hrefs.search('main', 'a b'), '#/s/main?q=a%20b');
    assert.equal(hrefs.catalogs('a&b'), '#/c?gone=a%26b');
});

test('hrefFor opens an index as its folder, a log or AGENTS.md as a document, the rest as a concept', () => {
    assert.deepEqual([...DOCUMENTS].sort(), ['AGENTS.md', 'index.md', 'log.md']);
    assert.equal(hrefFor('main', '/index.md'), '#/t/main');
    assert.equal(hrefFor('main', '/systems/index.md'), '#/t/main/systems');
    assert.equal(hrefFor('main', '/systems/log.md'), '#/d/main/systems/log.md');
    assert.equal(hrefFor('main', 'AGENTS.md'), '#/d/main/AGENTS.md');
    assert.equal(hrefFor('main', '/systems/alpha.md'), '#/v/main/systems/alpha.md');
    assert.equal(hrefFor('main', '/systems/alpha.md', 'notes'), '#/v/main/systems/alpha.md?s=notes');
    // A folder named like a concept is a folder like any other.
    assert.equal(hrefFor('main', '/archive.md/index.md'), '#/t/main/archive.md');
    // Only the exact names count.
    assert.equal(hrefFor('main', '/systems/my-index.md'), '#/v/main/systems/my-index.md');
    assert.equal(hrefFor('main', '/systems/Log.md'), '#/v/main/systems/Log.md');
});

test('fromBrowse turns the console\'s browse links into reader routes', () => {
    assert.equal(fromBrowse('#/browse/main/systems/beta.md'), '#/v/main/systems/beta.md');
    assert.equal(fromBrowse('#/browse/main/systems/beta.md/config'), '#/v/main/systems/beta.md?s=config');
    assert.equal(fromBrowse('#/browse/main/systems/alpha.md/caf%C3%A9'), '#/v/main/systems/alpha.md?s=caf%C3%A9');
    assert.equal(fromBrowse('#/browse/main/a%20b/c.md'), '#/v/main/a%20b/c.md');
    assert.equal(fromBrowse('#/browse/main/index.md'), '#/t/main');
    assert.equal(fromBrowse('#/browse/main/systems/index.md'), '#/t/main/systems');
    assert.equal(fromBrowse('#/browse/main/systems/log.md'), '#/d/main/systems/log.md');
    assert.equal(fromBrowse('#/browse/main/systems'), '#/t/main/systems');
});

test('fromBrowse ends the document at the last .md segment, so folders may be named x.md', () => {
    assert.equal(fromBrowse('#/browse/main/archive.md/topic.md'), '#/v/main/archive.md/topic.md');
    assert.equal(fromBrowse('#/browse/main/archive.md/topic.md/notes'), '#/v/main/archive.md/topic.md?s=notes');
    assert.equal(fromBrowse('#/browse/main/archive.md/index.md'), '#/t/main/archive.md');
    // The section as a parameter, which also names a folder 'x.md' unambiguously.
    assert.equal(fromBrowse('#/browse/main/systems/beta.md?s=config'), '#/v/main/systems/beta.md?s=config');
    assert.equal(fromBrowse('#/browse/main/archive.md/topic.md?s=notes'), '#/v/main/archive.md/topic.md?s=notes');
    assert.equal(fromBrowse('#/browse/main/a%20b/c.md?s=caf%C3%A9'), '#/v/main/a%20b/c.md?s=caf%C3%A9');
    assert.equal(fromBrowse('#/browse/main/archive.md/topic.md'), '#/v/main/archive.md/topic.md');
    assert.equal(fromBrowse('#/browse/main/a.md/b.md/log.md'), '#/d/main/a.md/b.md/log.md');
    assert.deepEqual(parse(fromBrowse('#/browse/main/archive.md/topic.md/notes')),
        { screen: 'concept', catalog: 'main', path: 'archive.md/topic.md', section: 'notes' });
});

test('a heading slug never ends in .md, so fromBrowse reads every slug as a section', () => {
    // The characters src/okf/markdown.rs `slugify` keeps: letters, digits, '_', '-'.
    for (const slug of ['notes', 'caf\u00e9', 'v12-release_notes', 'md', '-md', 'a-md']) {
        const route = parse(fromBrowse(`#/browse/main/archive.md/topic.md/${encodeURIComponent(slug)}`));
        assert.deepEqual([route.path, route.section], ['archive.md/topic.md', slug]);
    }
    // Only a #fragment ending in '.md' (which reaches no heading) reads as a longer path.
    assert.equal(parse(fromBrowse('#/browse/main/topic.md/notes.md')).path, 'topic.md/notes.md');
    // The parameter form has no such case: the section is never read as a path.
    assert.deepEqual(
        ((route) => [route.path, route.section])(parse(fromBrowse('#/browse/main/topic.md?s=notes.md'))),
        ['topic.md', 'notes.md']);
});

test('fromBrowse leaves every other link alone', () => {
    assert.equal(fromBrowse('https://example.com/x'), null);
    assert.equal(fromBrowse('#/v/main/a.md'), null);
    assert.equal(fromBrowse('#notes'), null);
});

test('parentOf is the folder holding a path', () => {
    assert.equal(parentOf('a/b/c.md'), 'a/b');
    assert.equal(parentOf('/a/b/c.md'), 'a/b');
    assert.equal(parentOf('c.md'), '');
    assert.equal(parentOf(''), '');
    assert.equal(parentOf(null), '');
});

const TREE = [
    { path: '/index.md', kind: 'index', title: 'Home' },
    { path: '/log.md', kind: 'log', title: null },
    { path: '/AGENTS.md', kind: 'agents', title: null },
    { path: '/zeta.md', kind: 'concept', title: 'Zeta' },
    { path: '/alpha.md', kind: 'concept', title: null },
    { path: '/systems/index.md', kind: 'index', title: 'Systems', description: 'What runs where.' },
    { path: '/systems/beta.md', kind: 'concept', title: 'Beta' },
    { path: '/systems/deep/gamma.md', kind: 'concept', title: 'Gamma' },
    { path: '/people/x.md', kind: 'concept', title: 'X' },
    { path: '/systemsx/y.md', kind: 'concept', title: 'Y' },
];

test('folderListing lists the root\'s folders, index, log and concepts by label', () => {
    const listing = folderListing(TREE, '');
    assert.deepEqual(listing.folders.map(entry => entry.name), ['people', 'systems', 'systemsx']);
    assert.equal(listing.index.path, '/index.md');
    assert.equal(listing.log.path, '/log.md');
    assert.deepEqual(listing.concepts.map(doc => doc.path), ['/alpha.md', '/zeta.md']);
    // Without a title, a concept is labelled by its file name.
    assert.equal(listing.label(listing.concepts[0]), 'alpha.md');
    assert.equal(listing.label(listing.concepts[1]), 'Zeta');
});

test('folderListing gives each subfolder its index title and description and its page count', () => {
    const systems = folderListing(TREE, '').folders.find(entry => entry.name === 'systems');
    // beta.md and deep/gamma.md: nested concepts count too.
    assert.deepEqual(systems, { name: 'systems', path: 'systems', title: 'Systems', description: 'What runs where.', pages: 2 });
    const [deep] = folderListing(TREE, 'systems').folders;
    assert.deepEqual(deep, { name: 'deep', path: 'systems/deep', title: null, description: null, pages: 1 });
});

test('folderListing lists one folder and none of its namesakes', () => {
    const listing = folderListing(TREE, 'systems');
    assert.deepEqual(listing.folders.map(entry => entry.name), ['deep']);
    assert.equal(listing.index.path, '/systems/index.md');
    assert.equal(listing.log, null);
    assert.deepEqual(listing.concepts.map(doc => doc.path), ['/systems/beta.md']);
});

test('folderListing of a folder without an index or documents is empty', () => {
    assert.deepEqual(folderListing(TREE, 'systems/deep').index, null);
    const empty = folderListing(TREE, 'nowhere');
    assert.deepEqual([empty.folders, empty.index, empty.log, empty.concepts], [[], null, null, []]);
});

test('folderTitles names every folder whose index has a title', () => {
    assert.deepEqual([...folderTitles(TREE)], [['systems', 'Systems']]);
});
