// A terminal row as tests read it, shared by the headless host and the browser
// fixture. The editor's shade is paint, not text: trailing cells that only
// carry a background read as unwritten, and a row that is nothing but one
// shade reads as the full-width rule the shade replaces, as the Rust harness
// projects it. Wide characters' trailing halves are text.
(function (root, factory) {
  if (typeof module === 'object' && module.exports) module.exports = factory();
  else root.shadeRows = factory();
})(this, () => {
  function rowText(line, cols) {
    let end = cols, background = null, uniform = true;
    for (; end > 0; end--) {
      const cell = line.getCell(end - 1);
      if (!cell || cell.getWidth() === 0 || cell.getChars().trim() !== '' || cell.isBgDefault()) break;
      const color = `${cell.getBgColorMode()}:${cell.getBgColor()}`;
      if (background === null) background = color;
      else if (background !== color) uniform = false;
    }
    if (end === 0 && uniform) return '─'.repeat(cols);
    return line.translateToString(true, 0, end);
  }
  return { rowText };
});
