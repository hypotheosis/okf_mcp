// A show/hide button for a password input (issue #1122). Shared by the
// console's account page and the server-rendered setup and reset pages.
import { h } from './dom.js';

// The text a label holds itself, without its controls' names.
function ownText(label) {
    return [...label.childNodes]
        .filter(node => node.nodeType === Node.TEXT_NODE)
        .map(node => node.textContent.trim())
        .join(' ')
        .trim();
}

// Puts a toggle next to `input` (the same element, moved into a wrapper, so
// its value, name and listeners stay) that switches its type between
// `password` and `text`. `field` names it for assistive technology, such
// as "new password". The input starts masked. Returns the button.
export function addReveal(input, field) {
    const button = h('button', { type: 'button', class: 'secondary reveal-toggle', dataset: { revealFor: field } });
    const draw = () => {
        const shown = input.type === 'text';
        button.textContent = shown ? 'Hide' : 'Show';
        button.setAttribute('aria-label', `${shown ? 'Hide' : 'Show'} ${field}`);
        button.setAttribute('aria-pressed', String(shown));
    };
    // The button sits inside the input's <label>, so without this its name
    // would become part of the input's.
    const label = input.closest('label');
    if (label && !input.hasAttribute('aria-label')) {
        input.setAttribute('aria-label', ownText(label) || field);
    }
    input.type = 'password';
    button.addEventListener('click', () => {
        input.type = input.type === 'password' ? 'text' : 'password';
        draw();
    });
    const wrapper = h('div', { class: 'reveal-field' });
    input.replaceWith(wrapper);
    wrapper.append(input, button);
    draw();
    return button;
}
