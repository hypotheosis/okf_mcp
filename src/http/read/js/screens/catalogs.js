// The catalog picker: the catalogs this account may read now (`readable`:
// the server leaves out a catalog an admin reaches but has not unlocked).
// With no catalog chosen yet and only one readable, it chooses that one and
// goes home.
import { h } from '/ui/js/core/dom.js';
import { readAll } from '../api.js';
import { hrefs } from '../route.js';
import { message, pageTitle } from '../parts.js';

export async function render(view, ctx) {
    const result = await readAll('/catalogs?readable=true', 'catalogs');
    if (!ctx.live()) return;
    ctx.freshness(result);
    const found = result.data;
    if (found.length === 1 && !ctx.state.catalog && !ctx.route.gone) {
        ctx.choose(found[0].name);
        location.replace(hrefs.home());
        return;
    }
    view.append(pageTitle('Catalogs'));
    if (ctx.route.gone) {
        view.append(message(`You can no longer read ${ctx.route.gone}. Its offline copies are deleted.`, null, 'catalog-gone'));
    }
    if (!found.length) {
        view.append(message('Your account reaches no catalog.', null, 'no-catalogs'));
        return;
    }
    view.append(h('div', { class: 'rows', id: 'catalog-list' }, found.map(catalog => {
        const label = catalog.label ?? catalog.name;
        return h('button', {
            type: 'button', class: 'row', dataset: { catalog: catalog.name },
            onclick: () => { ctx.choose(catalog.name); location.hash = hrefs.home(); },
        },
        h('span', { class: 'mark', 'aria-hidden': 'true' }, ([...label.trim()][0] ?? '?').toUpperCase()),
        h('span', { class: 'tx' },
            h('span', { class: 't' }, label),
            catalog.description ? h('span', { class: 's' }, catalog.description) : null),
        typeof catalog.concepts === 'number' ? h('span', { class: 'r' }, String(catalog.concepts)) : null);
    })));
}
