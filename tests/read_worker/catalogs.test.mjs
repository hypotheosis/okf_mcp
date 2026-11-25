// Tests of the catalog picker (src/http/read/js/screens/catalogs.js) run in
// Node (`just test-worker`): esm-hooks.mjs maps its imports to tree-doubles/
// (a fake DOM, a `readAll` the test answers), and `location` is a fake
// global, so the auto-choice, the notices, the buttons and an answer that
// arrives after the user has left can each be driven exactly.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { page } from './page.mjs';

const { render } = await page('screens/catalogs.js', '?doubles');

/** A promise with its `resolve`, settled by the test. */
function deferred() {
    let resolve;
    const promise = new Promise(yes => { resolve = yes; });
    return { promise, resolve };
}

const ticks = () => new Promise(resolve => setTimeout(resolve, 0));

const MAIN = { name: 'main', label: 'Main catalog', description: 'The main one' };
const OTHER = { name: 'other' };

/** The elements of `nodes` (and theirs) matching `test`. */
function find(nodes, test) {
    const found = [];
    const walk = node => {
        if (!node || typeof node !== 'object') return;
        if (test(node)) found.push(node);
        node.children?.forEach(walk);
    };
    nodes.forEach(walk);
    return found;
}

const byId = (nodes, id) => find(nodes, node => node.attrs?.id === id);
const text = node => (typeof node === 'string' ? node : (node.children ?? []).map(text).join(' '));

/**
 * One render of the picker: `answer` is what `readAll('/catalogs?readable=true')` gives
 * (a promise, or a list of catalogs), `state`/`route` the screen's. Spies
 * record every `choose`, `location.replace`, `location.hash` assignment and
 * freshness report.
 */
function scene({ answer, state = { catalog: null }, route = { gone: null } }) {
    const world = { appended: [], chosen: [], replaced: [], hashes: [], fresh: [], live: true };
    globalThis.treeWorld = {
        readAll: async (path, field) => {
            assert.deepEqual([path, field], ['/catalogs?readable=true', 'catalogs']);
            const found = await answer;
            return { data: found, offline: false };
        },
    };
    globalThis.location = {
        replace: hash => { world.replaced.push(hash); },
        set hash(value) { world.hashes.push(value); },
    };
    world.view = { append: (...nodes) => { world.appended.push(...nodes); } };
    world.ctx = {
        state, route,
        live: () => world.live,
        choose: catalog => { world.chosen.push(catalog); },
        freshness: result => { world.fresh.push(result); },
    };
    world.run = () => render(world.view, world.ctx);
    return world;
}

test('one catalog, none chosen and no notice: it is chosen once and the reader goes home', async () => {
    const world = scene({ answer: [MAIN] });
    await world.run();
    assert.deepEqual(world.chosen, ['main']);
    assert.deepEqual(world.replaced, ['#/']);
    assert.deepEqual(world.appended, [], 'the picker is never drawn');
});

test('the picker stays when the single catalog is not an automatic choice', async () => {
    const cases = [
        ['a catalog is already chosen', { state: { catalog: 'main' } }, null],
        ['a catalog was just lost', { route: { gone: 'old' } }, 'catalog-gone'],
    ];
    for (const [name, options, notice] of cases) {
        const world = scene({ answer: [MAIN], ...options });
        await world.run();
        assert.deepEqual(world.chosen, [], name);
        assert.deepEqual(world.replaced, [], name);
        assert.equal(byId(world.appended, 'catalog-list').length, 1, `${name}: lists it`);
        assert.match(text(find(world.appended, node => node.attrs?.dataset?.catalog === 'main')[0]), /Main catalog/, name);
        if (notice) assert.match(text(byId(world.appended, notice)[0]), /old/, name);
        else assert.equal(byId(world.appended, 'catalog-gone').length, 0, name);
    }
});

test('no catalog: the notice shows, nothing is chosen and nobody is redirected', async () => {
    const world = scene({ answer: [] });
    await world.run();
    assert.equal(byId(world.appended, 'no-catalogs').length, 1);
    assert.equal(byId(world.appended, 'catalog-list').length, 0);
    assert.deepEqual(world.chosen, []);
    assert.deepEqual(world.replaced, []);
});

test('several catalogs: each button chooses its own catalog and opens home', async () => {
    const world = scene({ answer: [MAIN, OTHER] });
    await world.run();
    const buttons = find(world.appended, node => node.attrs?.dataset?.catalog);
    assert.deepEqual(buttons.map(node => node.attrs.dataset.catalog), ['main', 'other']);
    assert.match(text(buttons[1]), /other/, 'a catalog without a label shows its name');
    assert.deepEqual(world.chosen, [], 'nothing is chosen before a click');
    for (const button of buttons) button.attrs.onclick();
    assert.deepEqual(world.chosen, ['main', 'other']);
    assert.deepEqual(world.hashes, ['#/', '#/']);
    assert.deepEqual(world.replaced, []);
});

test('an answer that arrives after the user has left draws, chooses and navigates nothing', async () => {
    for (const found of [[MAIN], [MAIN, OTHER], []]) {
        const answer = deferred();
        const world = scene({ answer: answer.promise });
        const rendering = world.run();
        await ticks();
        world.live = false;
        answer.resolve(found);
        await rendering;
        assert.deepEqual(world.appended, [], `${found.length} catalogs: nothing drawn`);
        assert.deepEqual(world.chosen, [], `${found.length} catalogs: nothing chosen`);
        assert.deepEqual(world.replaced, [], `${found.length} catalogs: no navigation`);
        assert.deepEqual(world.fresh, [], `${found.length} catalogs: no freshness`);
    }
});
