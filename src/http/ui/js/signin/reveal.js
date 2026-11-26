// The show/hide buttons of the setup and reset pages (src/http/signin.rs).
// Without this script the password fields stay masked and the forms submit
// as before; each input's data-reveal names its button.
import { addReveal } from '../core/reveal.js';

for (const input of document.querySelectorAll('input[data-reveal]')) {
    addReveal(input, input.dataset.reveal);
}
