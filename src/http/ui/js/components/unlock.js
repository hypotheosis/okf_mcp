// The Unlock form (M14 Task 9e): operator staff unlock a customer's catalog
// for themselves, with a category, a reason and a time limit, before they
// may open it (spec Part 6, "An unlock"). It posts /unlocks without `user`,
// so the unlock is the caller's own; the server refuses a credential with
// no user, or whose user is not an admin user (403; issue #447), hours over
// `unlock_max_hours` or 0 (422 `LimitExceeded`), and a catalog a scoped
// admin does not administer (404).
//
// The submit is a pending write keyed `unlock:<catalog>:create`
// (core/pending.js): while it runs, the form's Unlock is disabled, in this
// drawer and in one opened again for the same catalog meanwhile. A refusal
// keeps its reason in #unlock-form-error until the next submit answers, in
// whichever drawer shows that catalog's form when it settles. The form's
// two reads are side reads that never block it: the hour limit (GET
// /settings, 4 hours when it cannot be read) and, without a catalog, the
// catalogs to pick from (GET /catalogs, a text field when it cannot be read).
import { h } from '../core/dom.js';
import { api } from '../core/api.js';
import { store } from '../core/store.js';
import { toast } from '../core/toast.js';
import { when } from '../core/format.js';
import { drawerTicket } from '../core/drawer.js';
import { guard, once } from '../core/pending.js';
import { catalogLabel } from '../core/labels.js';
import { picker } from './picker.js';
import { refreshIdentity } from '../core/shell.js';

// The categories, in the server's form order (`Category::ALL`), labelled.
export const CATEGORIES = [
    ['support', 'Support'],
    ['deletion_request', 'Deletion request'],
    ['export_request', 'Export request'],
    ['security_investigation', 'Security investigation'],
    ['legal', 'Legal obligation'],
    ['other', 'Other'],
];
export const CATEGORY_LABEL = Object.fromEntries(CATEGORIES);

// The hour limit when the setting cannot be read (its default).
const DEFAULT_MAX_HOURS = 4;

// Whether this credential may ask for an unlock: an admin credential whose
// user is an admin user (whoami's `user_admin`; only operator staff make
// unlocks, issue #447). The server is the judge (it refuses anyone else, and
// the form shows why); this only decides whether to offer the button.
export function canUnlock() {
    return store.whoami?.access === 'admin' && !!store.whoami?.user && store.whoami?.user_admin === true;
}

// The pending-write key of an unlock of `catalog`.
const pendingKey = catalog => `unlock:${catalog}:create`;

// The form the drawer shows now (or last showed): {catalog(), ticket,
// report}. A submit that settles reports to it when it is the same
// catalog's, so a drawer closed and opened again meanwhile shows the answer.
let shownForm = null;

// `unlock_max_hours` from a settled GET /settings, or the default.
function maxHours(settled) {
    const row = settled.status === 'fulfilled'
        ? settled.value.settings.find(setting => setting.key === 'unlock_max_hours')
        : null;
    const hours = Number(row?.value);
    return Number.isInteger(hours) && hours > 0 ? hours : DEFAULT_MAX_HOURS;
}

// Opens the Unlock drawer.
//
// catalog: the catalog to unlock, or null to pick one in the form.
// onDone(view): called with the new unlock once the server made it.
// view: the page's container; the drawer's claim ends when it is detached
// (document.body, always attached, for a toast's Unlock).
export async function openUnlock(catalog, { onDone = () => {}, view = document.body } = {}) {
    const ticket = drawerTicket(view);
    // Settled one by one: a failed read falls back without losing the other.
    const [settings, catalogs] = await Promise.allSettled([
        api.get('/settings', { quiet: true }),
        catalog === null ? api.pages('/catalogs', 'catalogs', { quiet: true }) : Promise.resolve(null),
    ]);
    if (!ticket.current()) return;
    const max = maxHours(settings);
    let choose = null;
    if (catalog === null) {
        const names = catalogs.status === 'fulfilled' ? catalogs.value.catalogs.map(row => row.name) : null;
        // The catalog selected in the shell is picked to start with.
        choose = names
            ? picker({ name: 'catalog', required: true }, names, names.includes(store.catalog) ? store.catalog : null, catalogLabel)
            : h('input', { type: 'text', name: 'catalog', required: true, autocomplete: 'off',
                           placeholder: 'the catalog\'s name' });
    }
    const target = () => catalog ?? choose.value.trim();
    const submit = guard(h('button', { type: 'submit' }, 'Unlock'), pendingKey(target()));
    // The submit follows the catalog picked: its key names that catalog.
    if (choose) {
        for (const type of ['input', 'change']) choose.addEventListener(type, () => guard(submit, pendingKey(target())));
    }
    const category = h('select', { name: 'category', required: true },
        CATEGORIES.map(([value, label]) => h('option', { value }, label)));
    const reason = h('input', { type: 'text', name: 'reason', required: true, maxlength: '500',
                                autocomplete: 'off', placeholder: 'a ticket number, or what was asked' });
    const hours = h('input', { type: 'number', name: 'hours', required: true, min: '1', max: String(max),
                               step: '1', value: '1' });
    const refusal = h('p', { id: 'unlock-form-error', class: 'error small', role: 'alert', hidden: true });
    const form = h('form', { id: 'unlock-form' },
        choose ? h('label', {}, 'Catalog', choose) : null,
        h('label', {}, 'Category', category),
        h('label', {}, 'Reason', reason),
        h('label', {}, `Hours (at most ${max})`, hours),
        refusal,
        submit);

    // Shows the reason a submit was refused, or clears it (null).
    const report = text => {
        refusal.textContent = text ?? '';
        refusal.hidden = text === null;
    };
    const self = { catalog: target, ticket, report };
    shownForm = self;

    form.addEventListener('submit', event => {
        event.preventDefault();
        const name = target();
        once(pendingKey(name), async () => {
            let made = null;
            let refused = null;
            try {
                made = await api.post('/unlocks', {
                    catalog: name, category: category.value, reason: reason.value.trim(), hours: Number(hours.value),
                });
            } catch (error) {
                refused = error.message;   // already toasted
            }
            const shown = shownForm;
            const answer = shown?.catalog() === name && shown.ticket.current() ? shown : self;
            if (refused !== null) {
                answer.report(refused);
                return;
            }
            toast(`Unlocked ${made.catalog} until ${when(made.expires)}.`, 'ok');
            answer.ticket.close();
            // The catalog is readable now: the switcher offers it.
            await refreshIdentity().catch(() => {});
            onDone(made);
        });
    });

    ticket.open(catalog ? `Unlock ${catalog}` : 'Unlock a catalog',
        'Operator access to a customer\'s catalog, for you, until it expires or is ended. '
        + 'Its owners are told the category and the times, not the reason or who.',
        form);
}
