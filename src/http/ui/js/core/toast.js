import { h } from './dom.js';

// kind: info | ok | error. Errors stay longer and are announced as alerts.
// `lingerMs` overrides how long it stays (a trace id needs time to copy).
export function toast(message, kind = 'info', lingerMs = kind === 'error' ? 8000 : 4000) {
    const el = h('div', { class: `toast toast-${kind}`, role: kind === 'error' ? 'alert' : 'status' }, message);
    document.getElementById('toasts').append(el);
    setTimeout(() => el.remove(), lingerMs);
}
