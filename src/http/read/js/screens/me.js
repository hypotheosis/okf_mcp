// Account: who is signed in (read again on every open), until when, the theme choice, sign-out (which
// deletes every offline copy and the remembered catalog first, even if the
// server cannot be reached), and a local "Delete offline copies" that works
// with no connection at all. Both delete through main.js (`ctx.signOut`,
// `ctx.forget`).
import { h, fill } from '/ui/js/core/dom.js';
import { when } from '/ui/js/core/format.js';
import { message, pageTitle, sectionHead } from '../parts.js';
import { avatarClass, initials } from '../avatar.js';

const LABELS = Object.freeze({ system: 'System', light: 'Light', dark: 'Dark' });

function deleteOffline(view, ctx) {
    const button = h('button', { type: 'button', class: 'secondary', id: 'delete-offline' }, 'Delete offline copies');
    button.addEventListener('click', async () => {
        button.disabled = true;
        await ctx.forget();
        view.append(message('Offline copies are deleted from this device.', null, 'offline-deleted'));
    });
    return button;
}

// System, Light, Dark (theme.js); the current one pressed.
function appearance() {
    const theme = self.READ_THEME;
    const chips = theme.CHOICES.map(choice => h('button', {
        type: 'button', class: 'chip', id: `theme-${choice}`,
        'aria-pressed': String(theme.current() === choice),
        onclick: () => {
            theme.choose(choice);
            for (const chip of chips) chip.setAttribute('aria-pressed', String(chip.id === `theme-${choice}`));
        },
    }, LABELS[choice]));
    return [sectionHead('Appearance'), h('div', { class: 'chips', role: 'group', 'aria-label': 'Appearance' }, chips)];
}

export async function render(view, ctx) {
    // The session is read again each time Account opens: an online start
    // does not mean the connection is still there. A failed read leaves
    // ctx.state.session as it was.
    const status = await ctx.checkSession();
    // Left for another screen (or for sign-in) while the read was out: draw
    // nothing over it.
    if (!ctx.live() || status === ctx.SESSION.SIGNED_OUT) return;
    const session = status === ctx.SESSION.ANSWERED ? ctx.state.session : null;
    if (!session) {
        fill(view, pageTitle('Account'),
            message('Account details and sign-out need a connection.', null, 'account-offline'),
            appearance(),
            h('div', { class: 'actions' }, deleteOffline(view, ctx)));
        return;
    }
    const button = h('button', { type: 'button', id: 'sign-out' }, 'Sign out');
    button.addEventListener('click', async () => {
        button.disabled = true;
        // On success the page has left for sign-in.
        if (await ctx.signOut(session.csrf)) return;
        button.disabled = false;
        view.append(message('The server could not be reached to end the session. Offline copies are deleted.', null, 'sign-out-failed'));
    });
    // fill, not append: the email is null for an account without one.
    fill(view,
        pageTitle('Account'),
        h('div', { class: 'card' },
            h('span', { class: `avatar ${avatarClass(session.user)}`, 'aria-hidden': 'true' }, initials(session.user)),
            h('span', { class: 'tx' },
                h('span', {}, 'Signed in as ', h('strong', { id: 'account-user' }, session.user)),
                session.email ? h('span', { class: 's', id: 'account-email' }, session.email) : null)),
        h('p', { class: 'hint', id: 'account-expires' }, `This session ends by ${when(session.expires_at)}.`),
        appearance(),
        h('div', { class: 'actions' }, button, deleteOffline(view, ctx)));
}
