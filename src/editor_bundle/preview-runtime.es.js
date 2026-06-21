function E(e) {
  const n = (t) => {
    const a = t, l = window.getSelection();
    if (!l || l.isCollapsed || !a.clipboardData) return;
    const o = l.getRangeAt(0), r = o.cloneContents(), i = r.querySelector(".katex") !== null, s = !i && d(o.startContainer, e) !== null;
    if (!i && !s)
      return;
    if (s) {
      const p = d(o.startContainer, e), u = f(p);
      if (u === null) return;
      a.preventDefault(), a.clipboardData.setData("text/plain", u);
      return;
    }
    const c = x(r);
    c !== null && (a.preventDefault(), a.clipboardData.setData("text/plain", c));
  };
  return e.addEventListener("copy", n), () => e.removeEventListener("copy", n);
}
function x(e) {
  let n = "", t = !1, a = !0;
  function l(o) {
    if (o.nodeType === Node.TEXT_NODE) {
      n += o.textContent ?? "";
      return;
    }
    if (o.nodeType === Node.ELEMENT_NODE) {
      const r = o;
      if (r.classList.contains("katex")) {
        t = !0;
        const i = f(r);
        i !== null ? n += i : a = !1;
        return;
      }
    }
    if (o.nodeType === Node.ELEMENT_NODE || o.nodeType === Node.DOCUMENT_FRAGMENT_NODE)
      for (const r of Array.from(o.childNodes))
        l(r);
  }
  return l(e), t && !a ? null : n;
}
function d(e, n) {
  let t = e;
  for (; t && t !== n; ) {
    if (t.nodeType === Node.ELEMENT_NODE) {
      const a = t;
      if (a.classList.contains("katex")) return a;
    }
    t = t.parentNode;
  }
  return null;
}
function f(e) {
  const n = e.querySelector('annotation[encoding="application/x-tex"]');
  if (!n || !n.textContent) return null;
  const t = n.textContent.trim();
  return e.closest(".katex-display") !== null || e.getAttribute("data-display") === "true" ? `$$${t}$$` : `$${t}$`;
}
export {
  E as enableMathCopyAsTex
};
//# sourceMappingURL=preview-runtime.es.js.map
