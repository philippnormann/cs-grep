// Small DOM helpers shared by the page modules.

export const byId = (id) => document.getElementById(id);

export function el(tag, className = '', text = '') {
  const element = document.createElement(tag);
  element.className = className;
  element.textContent = text;
  return element;
}
