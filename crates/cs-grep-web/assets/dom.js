// Small DOM helpers shared by the page modules.

export const byId = (id) => document.getElementById(id);

// Sets page URL parameters in place, without a history entry; null removes one, while ''
// is kept, since `?venues=` means "no venues" and differs from no parameter at all.
export function setUrlParams(values) {
  const params = new URLSearchParams(location.search);
  for (const [key, value] of Object.entries(values)) {
    if (value == null) params.delete(key);
    else params.set(key, value);
  }
  // Shared links keep readable commas instead of %2C.
  const query = params.toString().replaceAll('%2C', ',');
  try {
    history.replaceState(null, '', `${location.pathname}${query ? `?${query}` : ''}${location.hash}`);
  } catch {
    // Safari limits URL updates to 100 per 10 seconds; the next change brings the URL up to date.
  }
}

export function el(tag, className = '', text = '') {
  const element = document.createElement(tag);
  element.className = className;
  element.textContent = text;
  return element;
}
