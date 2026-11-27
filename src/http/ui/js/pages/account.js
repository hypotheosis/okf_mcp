// Account: how the signed-in person signs in, and where. Profile (from /me
// on every load, never the sign-in snapshot) and ceiling (read-only, a page
// at a time from /me/ceiling, with a Find box: never the whole ceiling),
// password, passkeys (add, rename, remove), linked sign-in accounts (link,
// unlink), console sessions (sign any one out, this browser's included)
// and recent sign-ins. Each section loads and redraws on its own, only
// while the page is shown. The server refuses removing the last sign-in
// method (409, toasted with its reason). Changing the password, adding a
// passkey or linking an account signs the other sessions out, unless
// "Keep me signed in elsewhere" is ticked. "Delete your account" (issue
// #355) asks for the current password or a passkey, then DELETE /me; a
// refusal (the personal team owns catalogs, the last owner of a shared
// team, the last admin) shows its reason on the page and keeps the
// controls, and a deletion signs the console out.
import { h, fill, fact } from '../core/dom.js';
import { addReveal } from '../core/reveal.js';
import { api, enc } from '../core/api.js';
import { toast } from '../core/toast.js';
import { ago, when } from '../core/format.js';
import { freshAccount, refreshIdentity, requireSession, signedOut } from '../core/shell.js';
import { catalogLabel, named, teamLabel } from '../core/labels.js';
import { guard, once } from '../core/pending.js';
import { latest } from '../core/latest.js';
import { listTable } from '../components/list-table.js';
import { confirmDialog } from '../components/confirm.js';
import { accessPill } from '../components/levels.js';
import { ceilingPager } from '../components/ceiling.js';
import { sessionTable, signInTable } from '../components/activity.js';
import { assertionJson, createCredential, requestOptions } from '../signin/webauthn.js';

export async function render(main) {
    main.append(h('header', { class: 'page-head' },
        h('div', {},
            h('h1', {}, 'Account'),
            h('p', { class: 'muted' }, 'How you sign in, where you are signed in, and what your account may reach.'))));
    if (!requireSession(main, 'account-denied')) return;
    const profile = section('account-profile', 'Profile');
    const owned = section('account-owned-teams', 'Teams you own',
        'Rename the shared teams you own. Everyone who sees a team sees this name.');
    const ceiling = section('account-ceiling', 'Your ceiling',
        'The most a key or OAuth grant you create can reach: the catalogs of your teams, at each catalog\'s '
        + 'member access, and the catalogs an admin entitled you to; where both apply, the higher level. '
        + 'An admin changes it.');
    const password = section('account-password', 'Password');
    const passkeys = section('account-passkeys', 'Passkeys', 'Sign in with your device instead of a password.');
    const identities = section('account-identities', 'Linked accounts', 'Sign in through another identity provider.');
    const sessions = section('account-sessions', 'Where you are signed in');
    const signIns = section('account-sign-ins', 'Recent sign-ins');
    const removal = section('account-deletion', 'Delete your account',
        'Deleting your account removes your keys, OAuth grants, sessions, passkeys, linked accounts and your '
        + 'personal team, at once and for good. Catalogs and their history stay; the audit log keeps its '
        + 'entries, naming you only as a deleted user.');
    main.append(profile.el, owned.el, ceiling.el, password.el, passkeys.el, identities.el, sessions.el, signIns.el,
        removal.el);
    // What the delete section is drawn from: the account (whether it has a
    // password) and how many passkeys it has, each set by its own read;
    // `null` until that read answers. `deleteRefusal` is the last refusal's
    // reason, kept across redraws until the next attempt.
    // One counter per list: a read that a newer one overtook draws nothing.
    const passkeyReads = latest(main);
    const identityReads = latest(main);
    const sessionReads = latest(main);
    let deleteAccount = null;
    let deletePasskeys = null;
    let deleteRefusal = null;
    // The last refusal was `ReauthStale`: the provider did not vouch for a
    // fresh sign-in, so the retry button comes with sign-out-and-in guidance.
    let deleteStale = false;
    // The linked accounts the provider re-authentication may use (a linked
    // account whose provider is still configured), `null` until the
    // identities read answers; and the provider the browser just came back
    // from, remembered across the redirect in sessionStorage.
    let deleteReauth = null;
    let reauthedWith = takeReauthMarker();
    // What the server says waits for this session after that return:
    // `ready`, `stale` (the provider did not vouch for a fresh sign-in),
    // `none` (nothing waits any more) or `unknown` (the status call failed,
    // so the server decides at deletion); `null` until it answers. It comes
    // from a read-only call that never spends the proof.
    let reauthState = null;
    // The ceiling table, a page at a time: drawn once, then redrawn with
    // each committed page. `shownPrefix` is the prefix the drawn rows
    // answer (null before the first page), for the empty text.
    let shownPrefix = null;
    const ceilingTable = listTable([
        { key: 'catalog', label: 'Catalog',
          render: row => h('span', { dataset: { test: `ceiling-${row.catalog}` } },
              named(catalogLabel(row.catalog), row.catalog)) },
        { key: 'access', label: 'Access', render: row => accessPill(row.access) },
    ], [], { empty: () => {
        if (shownPrefix === null) return 'Loading...';
        return shownPrefix ? `No catalog of yours starts with ${shownPrefix}.` : 'Your account reaches no catalog yet. Ask an admin.';
    } });
    ceilingTable.id = 'ceiling-table';
    const ceilingPage = ceilingPager({ id: 'ceiling', draw: (rows, prefix) => {
        shownPrefix = prefix;
        ceilingTable.redraw(rows);
    } });
    fill(ceiling.body, h('label', { class: 'small' }, 'Find ', ceilingPage.find), ceilingTable, ceilingPage.footer);

    async function drawAccount() {
        let account;
        try { account = await freshAccount(); } catch { return; /* toasted */ }
        if (!main.isConnected) return;
        fill(profile.body, h('dl', { class: 'facts', id: 'account-facts' },
            fact('Display name', displayNameForm(account)),
            fact('Email', account.email ?? 'none'),
            fact('Teams', account.teams.length ? account.teams.map(teamLabel).join(', ') : '-'),
            fact('Admin', account.admin ? 'yes: you sign in to the full console' : 'no')));
        // The slug stays visible as an id, never as the person's name.
        profile.body.append(h('p', { id: 'account-id', class: 'muted id-line' },
            'Account ID: ', h('code', {}, account.user)));
        fill(owned.body, ownedTeams(account.owned_team_details ?? []));
        owned.el.hidden = !(account.owned_team_details ?? []).length;
        fill(password.body, passwordForm(account));
        deleteAccount = account;
        drawDelete();
    }

    // The display-name editor: the label is user text, shown in an input's
    // value (never markup). The server answers a too-long or control-
    // character name with its reason (toasted by api.js).
    function displayNameForm(account) {
        const key = 'account:display-name';
        const input = h('input', { id: 'account-display-name', autocomplete: 'off',
                                   'aria-label': 'Display name', value: account.display_name,
                                   placeholder: account.label });
        const save = guard(h('button', { type: 'button', id: 'account-display-name-save' }, 'Save'), key);
        save.addEventListener('click', () => once(key, async () => {
            try { await api.patch('/me', { display_name: input.value }); } catch { return; /* toasted */ }
            toast('Display name saved', 'ok');
            // The header and every label come from the identity read.
            try { await refreshIdentity(); } catch { /* the next load shows it */ }
            if (main.isConnected) await drawAccount();
        }));
        return h('div', { class: 'inline-form' }, input, save);
    }

    // One row per team the user owns: an editor for a shared team, a note
    // for the personal one (whose label follows the user). The Teams page
    // is admin-only, so this is how an owner who is not an admin renames.
    function ownedTeams(details) {
        return h('ul', { class: 'grant-list', id: 'account-owned-list' }, details.map(team => {
            if (team.personal) {
                return h('li', { dataset: { test: `owned-team-${team.name}` } },
                    h('span', {}, team.label), ' ', h('span', { class: 'muted small' }, 'Named after you'));
            }
            const key = `account:team:${team.name}`;
            const input = h('input', { id: `account-team-${team.name}`, autocomplete: 'off',
                                       'aria-label': `Display name of ${team.name}`, value: team.display_name,
                                       placeholder: team.label });
            const save = guard(h('button', { type: 'button', id: `account-team-${team.name}-save` }, 'Save'), key);
            save.addEventListener('click', () => once(key, async () => {
                try {
                    await api.patch(`/teams/${enc(team.name)}`, { display_name: input.value });
                } catch { return; /* toasted */ }
                toast('Team name saved', 'ok');
                try { await refreshIdentity(); } catch { /* the next load shows it */ }
                if (main.isConnected) await drawAccount();
            }));
            return h('li', { dataset: { test: `owned-team-${team.name}` } },
                h('div', { class: 'inline-form' }, input, save),
                h('span', { class: 'id-line muted' }, team.name));
        }));
    }

    function passwordForm(account) {
        const current = account.has_password
            ? h('input', { type: 'password', name: 'current', required: true, autocomplete: 'current-password' })
            : null;
        const next = h('input', { type: 'password', name: 'password', required: true, minlength: '12', autocomplete: 'new-password' });
        const repeat = h('input', { type: 'password', name: 'confirm', required: true, minlength: '12', autocomplete: 'new-password' });
        const form = h('form', { id: 'password-form' },
            account.password_sign_in ? null : h('p', { class: 'muted small', id: 'password-sign-in-off' },
                'Password sign-in is turned off on this server: a password works once an admin turns it back on.'),
            current
                ? h('label', {}, 'Current password', current)
                : h('p', { class: 'muted small' }, 'You have no password yet. Set one to sign in with your email.'),
            h('label', {}, 'New password', next),
            h('label', {}, 'Repeat the new password', repeat),
            h('p', { class: 'muted small' }, 'At least 12 characters, and not a common password.'),
            keepSessionsBox('password-keep-sessions'),
            h('button', { type: 'submit' }, account.has_password ? 'Change password' : 'Set password'));
        // Each field gets its own button, so both new ones can be shown to compare.
        if (current) addReveal(current, 'current password');
        addReveal(next, 'new password');
        addReveal(repeat, 'new password confirmation');
        form.addEventListener('submit', async event => {
            event.preventDefault();
            const data = new FormData(form);
            const typed = String(data.get('password') ?? '');
            if (typed !== String(data.get('confirm') ?? '')) {
                toast('The two new passwords differ.', 'error');
                return;
            }
            try {
                // A wrong current password is a 403, never a 401, so the
                // console stays signed in; the policy's refusal is a 422.
                await api.put('/me/password', {
                    current_password: current ? current.value : null,
                    new_password: typed,
                    keep_sessions: data.get('keep_sessions') === 'on',
                }, { quiet: true });
            } catch (error) {
                if (error.status === 401) return; // signed out by the 401 handler
                toast(`The password was not changed: ${reason(error)}.`, 'error');
                if (current && error.status === 403) {
                    current.value = '';
                    current.focus();
                }
                return;
            }
            toast(account.has_password ? 'Password changed.' : 'Password set.', 'ok');
            if (main.isConnected) await Promise.all([drawAccount(), drawSessions()]);
        });
        return form;
    }

    async function drawPasskeys() {
        const current = passkeyReads();
        let list;
        try { ({ passkeys: list } = await api.get('/me/passkeys')); } catch { return; }
        if (!current()) return;
        deletePasskeys = list.length;
        drawDelete();
        const name = h('input', { id: 'passkey-name', 'aria-label': 'A name for the new passkey',
                                  placeholder: 'a name, such as laptop', autocomplete: 'off' });
        const keep = keepSessionsBox('passkey-keep-sessions');
        const add = h('button', { type: 'button', id: 'passkey-add' }, 'Add a passkey');
        add.addEventListener('click', async () => {
            add.disabled = true;
            try {
                await addPasskey(name.value.trim() || 'passkey', keep.querySelector('input').checked);
            } finally {
                add.disabled = false;
            }
        });
        fill(passkeys.body,
            h('ul', { class: 'grant-list', id: 'passkey-list' },
                list.length ? list.map(passkeyRow) : h('li', { class: 'muted' }, 'No passkeys yet.')),
            h('div', { class: 'inline-form' }, name, add),
            keep);
    }

    function passkeyRow(passkey) {
        const base = `/me/passkeys/${enc(passkey.id)}`;
        const label = h('input', { value: passkey.name, autocomplete: 'off', 'aria-label': `New name for ${passkey.name}` });
        const rename = async () => {
            const next = label.value.trim();
            if (!next || next === passkey.name) return;
            try { await api.patch(base, { name: next }); } catch { return; /* toasted */ }
            toast(`Renamed passkey ${passkey.name} to ${next}.`, 'ok');
            if (main.isConnected) await drawPasskeys();
        };
        const remove = async () => {
            const ok = await confirmDialog({
                title: `Remove passkey ${passkey.name}?`,
                message: 'You can no longer sign in with it. Remove it from the device too.',
                action: 'Remove',
            });
            if (!ok) return;
            try { await api.del(base); } catch { return; /* toasted: 409 says it is the last sign-in method */ }
            toast(`Removed passkey ${passkey.name}.`, 'ok');
            if (main.isConnected) await drawPasskeys();
        };
        return h('li', { dataset: { test: `passkey-${passkey.name}` } },
            label,
            h('button', { type: 'button', class: 'secondary', dataset: { test: `passkey-rename-${passkey.name}` }, onclick: rename }, 'Rename'),
            h('span', { class: 'muted small' },
                `added ${when(passkey.created_at)}, last used ${passkey.last_used_at ? ago(passkey.last_used_at) : 'never'}`),
            h('button', { type: 'button', class: 'text-btn', dataset: { test: `passkey-remove-${passkey.name}` }, onclick: remove }, 'Remove'));
    }

    // Registration (Task 8): the server's challenge (start takes the name
    // and the keep-sessions choice, and sets the ceremony cookie), the
    // browser's prompt, then the credential back to the server. A cancelled
    // or timed-out prompt is a calm note, never an error or a script error.
    async function addPasskey(name, keep) {
        let challenge;
        try {
            challenge = await api.post('/me/passkeys/register/start', { name, keep_sessions: keep });
        } catch { return; /* toasted */ }
        let credential;
        try {
            credential = await createCredential(challenge);
        } catch (error) {
            if (error?.name === 'NotAllowedError') {
                toast('No passkey was added: the browser prompt was cancelled or timed out.', 'info');
            } else {
                toast(`No passkey was added: ${error?.message ?? error}`, 'error');
            }
            return;
        }
        try {
            await api.post('/me/passkeys/register/finish', credential, { quiet: true });
        } catch (error) {
            // 403: another session started this ceremony; 409: a passkey
            // already registered; 410: the ceremony expired; 422: a
            // credential the server could not verify. Only a 401 (this
            // session ended) signs the console out.
            if (error.status !== 401) toast(`No passkey was added: ${reason(error)}.`, 'error');
            return;
        }
        toast(`Added passkey ${name}.`, 'ok');
        if (main.isConnected) await Promise.all([drawPasskeys(), drawSessions()]);
    }

    async function drawIdentities() {
        let linked;
        let providers;
        const current = identityReads();
        try { ({ identities: linked, providers } = await api.get('/me/identities')); } catch { return; }
        if (!current()) return;
        const label = id => providers.find(provider => provider.id === id)?.label ?? id;
        const held = new Set(linked.map(identity => identity.provider));
        deleteReauth = linked
            .filter(identity => providers.some(provider => provider.id === identity.provider))
            .map(identity => ({ id: identity.provider, label: label(identity.provider) }));
        drawDelete();
        const keep = keepSessionsBox('identity-keep-sessions');
        fill(identities.body,
            h('ul', { class: 'grant-list', id: 'identity-list' }, linked.length
                ? linked.map(identity => h('li', { dataset: { test: `identity-${identity.provider}` } },
                    h('strong', {}, label(identity.provider)), ' ',
                    h('span', { class: 'mono small' }, identity.email_at_link),
                    h('span', { class: 'muted small' }, ` linked ${when(identity.created_at)}`),
                    h('button', {
                        type: 'button', class: 'text-btn', dataset: { test: `identity-unlink-${identity.provider}` },
                        onclick: () => unlink(identity.provider, label(identity.provider)),
                    }, 'Unlink')))
                : h('li', { class: 'muted' }, 'No linked accounts.')),
            providers.filter(provider => !held.has(provider.id)).map(provider => h('button', {
                type: 'button', class: 'secondary', dataset: { test: `identity-link-${provider.id}` },
                onclick: () => link(provider.id, keep.querySelector('input').checked),
            }, `Link ${provider.label}`)),
            providers.length ? keep : h('p', { class: 'muted small' }, 'This server has no sign-in providers configured.'));
    }

    // Linking leaves the console (Task 9): the server answers where to send
    // the browser (`{url}`, the provider's sign-in page, with the flow
    // cookie set), and its callback comes back to /ui/#/account.
    async function link(provider, keep) {
        let answer;
        try {
            answer = await api.post(`/me/identities/${enc(provider)}/link`, { keep_sessions: keep });
        } catch { return; /* toasted */ }
        location.assign(answer.url);
    }

    async function unlink(provider, name) {
        const ok = await confirmDialog({
            title: `Unlink ${name}?`,
            message: `You can no longer sign in through ${name} until you link it again.`,
            action: 'Unlink',
        });
        if (!ok) return;
        try { await api.del(`/me/identities/${enc(provider)}`); } catch { return; /* toasted: 409 for the last method */ }
        toast(`Unlinked ${name}.`, 'ok');
        if (main.isConnected) await drawIdentities();
    }

    async function drawSessions() {
        const current = sessionReads();
        let list;
        try { ({ sessions: list } = await api.get('/me/sessions')); } catch { return; }
        if (!current()) return;
        // One Sign out per session (Task 10b has no "all others" route).
        // This browser's own signs the console out once the server ended it.
        fill(sessions.body,
            sessionTable({
                id: 'session-table', prefix: 'session', sessions: list, revokeCurrent: true,
                revoke: entry => (entry.current ? signOutHere(entry) : signOutOther(entry)),
            }));
    }

    async function signOutOther(entry) {
        try { await api.del(`/me/sessions/${enc(entry.id_prefix)}`); } catch { return; /* toasted */ }
        toast('Signed that session out.', 'ok');
        if (main.isConnected) await drawSessions();
    }

    async function signOutHere(entry) {
        const ok = await confirmDialog({
            title: 'Sign this browser out?',
            message: 'This ends the session you are using now. Sign in again to come back.',
            action: 'Sign out',
        });
        if (!ok) return;
        // A 401 means it had ended already: the 401 handler shows the card.
        try { await api.del(`/me/sessions/${enc(entry.id_prefix)}`); } catch { return; /* toasted */ }
        signedOut();
    }

    // The delete section: a password box when the account has a password,
    // a passkey button when it has a passkey, else a note saying how to get
    // either. Both controls share one pending key, so a second click (or a
    // redraw while the request runs) never sends a second deletion. No
    // control is drawn until the account read has answered: the typed
    // confirmation needs its id, whichever read answers first.
    function drawDelete() {
        if (!main.isConnected) return;
        if (deleteAccount === null) {
            fill(removal.body, h('p', { class: 'muted small', id: 'account-delete-loading' },
                'Loading your account... Deleting it needs your account ID, so this waits for it.'));
            return;
        }
        const key = 'account:delete';
        // Back from a provider that did not vouch for a fresh sign-in: the
        // guidance shows now, before any deletion attempt.
        const returned = (deleteReauth ?? []).find(identity => identity.id === reauthedWith);
        const staleBack = returned !== undefined && reauthState === 'stale';
        const shown = deleteRefusal ?? (staleBack ? staleGuidance(returned.label) : '');
        const refusal = h('p', { id: 'account-delete-reason', class: 'error', role: 'alert' }, shown);
        refusal.hidden = !shown;
        if (deleteStale || staleBack) refusal.dataset.kind = 'ReauthStale';
        const parts = [refusal];
        if (deleteAccount?.has_password) {
            const typed = h('input', { type: 'password', id: 'account-delete-password', autocomplete: 'current-password',
                                       'aria-label': 'Your current password' });
            const go = guard(h('button', { type: 'button', id: 'account-delete', class: 'danger' },
                'Delete my account'), key);
            go.addEventListener('click', () => removeAccount(key, () => ({ password: typed.value })));
            parts.push(h('label', {}, 'Current password', typed), go);
        }
        if (deletePasskeys) {
            const go = guard(h('button', { type: 'button', id: 'account-delete-passkey', class: 'danger' },
                'Confirm with a passkey and delete my account'), key);
            go.addEventListener('click', () => removeAccount(key, passkeyProof));
            parts.push(h('div', { class: 'inline-form' }, go));
        }
        for (const identity of deleteReauth ?? []) {
            if (reauthedWith === identity.id && reauthState === null) {
                parts.push(h('p', { class: 'muted small', id: 'account-delete-checking' },
                    `Checking your re-authentication with ${identity.label}...`));
            } else if (reauthedWith === identity.id && reauthState !== 'stale' && reauthState !== 'none') {
                // Back from the provider: the server holds a single-use
                // proof for this session, good for a few minutes.
                const go = guard(h('button', { type: 'button', id: 'account-delete-oidc', class: 'danger' },
                    'Delete my account'), key);
                go.addEventListener('click', () => removeAccount(key, () => ({ oidc_reauth: true })));
                parts.push(h('p', { class: 'small', id: 'account-delete-reauthed' },
                    `You re-authenticated with ${identity.label}. Delete your account within a few minutes, `
                    + 'or re-authenticate again.'), go);
            } else {
                const go = guard(h('button', { type: 'button', id: `account-delete-reauth-${identity.id}`,
                                               class: 'danger' },
                    deleteStale || staleBack
                        ? `Try again: re-authenticate with ${identity.label}`
                        : `Re-authenticate with ${identity.label} to delete my account`), key);
                go.addEventListener('click', () => reauthenticate(key, identity.id));
                parts.push(h('div', { class: 'inline-form' }, go));
            }
        }
        if (deleteAccount && !deleteAccount.has_password && deletePasskeys === 0 && deleteReauth?.length === 0) {
            parts.push(h('p', { class: 'muted small', id: 'account-delete-needs-proof' },
                'Set a password, add a passkey or link a sign-in account first: deleting your account asks for '
                + 'one to confirm it is you.'));
        }
        fill(removal.body, ...parts);
    }

    // The provider proof: the server answers where to send the browser
    // (`{url}`, the provider's sign-in page asked for a fresh login, with the
    // flow cookie set), and its callback comes back to /ui/#/account, where
    // the delete section then offers the deletion itself. The typed
    // confirmation comes after the return, with the deletion.
    async function reauthenticate(key, provider) {
        await once(key, async () => {
            deleteStale = false;
            // A new attempt replaces the one the status described.
            reauthedWith = null;
            let answer;
            try {
                answer = await api.post(`/me/identities/${enc(provider)}/reauth`, {}, { quiet: true });
            } catch (error) {
                if (error.status === 401) return;
                deleteRefusal = `Re-authentication did not start: ${reason(error)}.`;
                drawDelete();
                return;
            }
            rememberReauth(provider);
            location.assign(answer.url);
        });
        if (main.isConnected) drawDelete();
    }

    // The passkey proof: the server's challenge for this account's own
    // passkeys, the browser's prompt, and its assertion. Rejects with the
    // refusal to show.
    async function passkeyProof() {
        const challenge = await api.post('/me/passkeys/confirm/start', {}, { quiet: true });
        let credential;
        try {
            credential = await navigator.credentials.get(requestOptions(challenge));
        } catch (error) {
            throw new Error(error?.name === 'NotAllowedError'
                ? 'the passkey prompt was cancelled or timed out'
                : String(error?.message ?? error));
        }
        return { passkey: assertionJson(credential) };
    }

    // Confirms, re-authenticates and deletes. A refusal keeps the page and
    // its controls, with the reason shown; a 401 means this session ended
    // (the 401 handler shows the sign-in card); a deletion signs out.
    async function removeAccount(key, proof) {
        // The typed confirmation is never skipped: without the account id
        // there is nothing to type, so there is no deletion.
        const id = deleteAccount?.user;
        if (!id) return;
        const ok = await confirmDialog({
            title: 'Delete your account?',
            message: 'This cannot be undone. You are signed out everywhere, and your keys and grants stop working.',
            action: 'Delete my account',
            typed: id,
        });
        if (!ok) return;
        await once(key, async () => {
            try {
                await api.del('/me', { body: await proof(), quiet: true });
            } catch (error) {
                if (error.status === 401) return;
                // The server spends a provider proof whatever the answer:
                // a retry starts from a new re-authentication.
                reauthedWith = null;
                // Only this kind tells the person to sign out of the provider
                // and back in; every other refusal stays generic.
                deleteStale = error.body?.error === 'ReauthStale';
                deleteRefusal = `Your account was not deleted: ${reason(error)}.`;
                drawDelete();
                return;
            }
            signedOut('Your account was deleted.');
        });
        // Drawn again once the write settled, so the controls are usable.
        if (main.isConnected) drawDelete();
    }

    // After the return from the provider, asks the server what waits for
    // this session (a read-only call: it never spends the proof), so a
    // stale outcome shows its guidance at once and the delete button only
    // comes when a fresh proof waits. A failed call leaves the old
    // behaviour: the button, and the deletion's own answer.
    async function checkReauth() {
        const provider = reauthedWith;
        if (!provider) return;
        let state;
        try {
            ({ state } = await api.get(`/me/identities/${enc(provider)}/reauth`, { quiet: true }));
        } catch (error) {
            if (error.status === 401) return;
            state = 'unknown';
        }
        if (!main.isConnected || reauthedWith !== provider) return;
        reauthState = state;
        drawDelete();
    }

    async function drawSignIns() {
        let list;
        try { ({ sign_ins: list } = await api.get('/me/sign-ins')); } catch { return; }
        if (main.isConnected) fill(signIns.body, signInTable({ id: 'sign-in-table', signIns: list }));
    }

    await Promise.all([drawAccount(), ceilingPage.start(), drawPasskeys(), drawIdentities(), drawSessions(), drawSignIns(), checkReauth()]);
    // Every section has now drawn (each draw settles on its own, a failed
    // read included). The sections load in any order and each one moves the
    // controls below it, so a browser journey waits for this marker
    // (`.view[data-loaded=true]`) before it clicks, instead of for the one
    // control it wants: that control can appear while the rest of the page
    // is still arriving (issue #373).
    if (main.isConnected) main.dataset.loaded = 'true';
}

// The provider the person re-authenticated with before the redirect, kept
// for this tab only. It only says which button to draw: the server decides
// whether a proof exists (a missing or expired one answers 410).
const REAUTH_MARKER = 'okf:delete-reauth';

function rememberReauth(provider) {
    try { sessionStorage.setItem(REAUTH_MARKER, provider); } catch { /* storage may be blocked */ }
}

// Reads and clears the marker, so a later visit starts from the plain
// buttons.
function takeReauthMarker() {
    try {
        const provider = sessionStorage.getItem(REAUTH_MARKER);
        sessionStorage.removeItem(REAUTH_MARKER);
        return provider;
    } catch {
        return null;
    }
}

// The guidance for a provider that did not vouch for a fresh sign-in, as
// `DELETE /api/v1/me` words it (`OkfError::ReauthStale`), shown on the
// return from the provider before any deletion is tried.
function staleGuidance(label) {
    return `${label} did not confirm a fresh sign-in, so the account cannot be deleted yet; `
        + `sign out of ${label}, sign back in, then try again`;
}

// A refusal's reason as the server gave it, without the "forbidden: "
// prefix its 403s carry.
function reason(error) {
    return String(error?.body?.message ?? error?.message ?? error).replace(/^forbidden: /, '');
}

// A titled section of the page and the body its draw function fills.
function section(id, title, intro = null) {
    const body = h('div', { class: 'account-body' });
    const el = h('section', { id, class: 'account-section' },
        h('h2', {}, title), intro ? h('p', { class: 'muted small' }, intro) : null, body);
    return { el, body };
}

// The "keep my other sessions" box: unticked, a new sign-in method signs
// every other session out.
function keepSessionsBox(id) {
    return h('label', { class: 'small' },
        h('input', { type: 'checkbox', id, name: 'keep_sessions' }), ' Keep me signed in elsewhere');
}
