var Kt = Object.defineProperty;
var Zt = (e, t, n) => t in e ? Kt(e, t, { enumerable: !0, configurable: !0, writable: !0, value: n }) : e[t] = n;
var M = (e, t, n) => Zt(e, typeof t != "symbol" ? t + "" : t, n);
import * as u from "yjs";
import { E as yt, V as rt, F as Et, A as wt, a as it, R as Qt, D as x, W as te } from "./index-xTTurzIe.js";
class at {
  /**
   * @param {Y.RelativePosition} yanchor
   * @param {Y.RelativePosition} yhead
   */
  constructor(t, n) {
    this.yanchor = t, this.yhead = n;
  }
  /**
   * @returns {any}
   */
  toJSON() {
    return {
      yanchor: u.relativePositionToJSON(this.yanchor),
      yhead: u.relativePositionToJSON(this.yhead)
    };
  }
  /**
   * @param {any} json
   * @return {YRange}
   */
  static fromJSON(t) {
    return new at(u.createRelativePositionFromJSON(t.yanchor), u.createRelativePositionFromJSON(t.yhead));
  }
}
class ee {
  constructor(t, n) {
    this.ytext = t, this.awareness = n, this.undoManager = new u.UndoManager(t);
  }
  /**
   * Helper function to transform an absolute index position to a Yjs-based relative position
   * (https://docs.yjs.dev/api/relative-positions).
   *
   * A relative position can be transformed back to an absolute position even after the document has changed. The position is
   * automatically adapted. This does not require any position transformations. Relative positions are computed based on
   * the internal Yjs document model. Peers that share content through Yjs are guaranteed that their positions will always
   * synced up when using relatve positions.
   *
   * ```js
   * import { ySyncFacet } from 'y-codemirror'
   *
   * ..
   * const ysync = view.state.facet(ySyncFacet)
   * // transform an absolute index position to a ypos
   * const ypos = ysync.getYPos(3)
   * // transform the ypos back to an absolute position
   * ysync.fromYPos(ypos) // => 3
   * ```
   *
   * It cannot be guaranteed that absolute index positions can be synced up between peers.
   * This might lead to undesired behavior when implementing features that require that all peers see the
   * same marked range (e.g. a comment plugin).
   *
   * @param {number} pos
   * @param {number} [assoc]
   */
  toYPos(t, n = 0) {
    return u.createRelativePositionFromTypeIndex(this.ytext, t, n);
  }
  /**
   * @param {Y.RelativePosition | Object} rpos
   */
  fromYPos(t) {
    const n = u.createAbsolutePositionFromRelativePosition(u.createRelativePositionFromJSON(t), this.ytext.doc);
    if (n == null || n.type !== this.ytext)
      throw new Error("[y-codemirror] The position you want to retrieve was created by a different document");
    return {
      pos: n.index,
      assoc: n.assoc
    };
  }
  /**
   * @param {cmState.SelectionRange} range
   * @return {YRange}
   */
  toYRange(t) {
    const n = t.assoc, s = this.toYPos(t.anchor, n), o = this.toYPos(t.head, n);
    return new at(s, o);
  }
  /**
   * @param {YRange} yrange
   */
  fromYRange(t) {
    const n = this.fromYPos(t.yanchor), s = this.fromYPos(t.yhead);
    return n.pos === s.pos ? yt.cursor(s.pos, s.assoc) : yt.range(n.pos, s.pos);
  }
}
const I = Et.define({
  combine(e) {
    return e[e.length - 1];
  }
}), et = wt.define();
class ne {
  /**
   * @param {cmView.EditorView} view
   */
  constructor(t) {
    this.view = t, this.conf = t.state.facet(I), this._observer = (n, s) => {
      if (s.origin !== this.conf) {
        const o = n.delta, c = [];
        let r = 0;
        for (let f = 0; f < o.length; f++) {
          const l = o[f];
          l.insert != null ? c.push({ from: r, to: r, insert: l.insert }) : l.delete != null ? (c.push({ from: r, to: r + l.delete, insert: "" }), r += l.delete) : r += l.retain;
        }
        t.dispatch({ changes: c, annotations: [et.of(this.conf)] });
      }
    }, this._ytext = this.conf.ytext, this._ytext.observe(this._observer);
  }
  /**
   * @param {cmView.ViewUpdate} update
   */
  update(t) {
    if (!t.docChanged || t.transactions.length > 0 && t.transactions[0].annotation(et) === this.conf)
      return;
    const n = this.conf.ytext;
    n.doc.transact(() => {
      let s = 0;
      t.changes.iterChanges((o, c, r, f, l) => {
        const d = l.sliceString(0, l.length, `
`);
        o !== c && n.delete(o + s, c - o), d.length > 0 && n.insert(o + s, d), s += d.length - (c - o);
      });
    }, this.conf);
  }
  destroy() {
    this._ytext.unobserve(this._observer);
  }
}
const se = rt.fromClass(ne);
class oe {
  /**
   * @param {L} left
   * @param {R} right
   */
  constructor(t, n) {
    this.left = t, this.right = n;
  }
}
const R = (e, t) => new oe(e, t), ce = (e, t) => e.forEach((n) => t(n.left, n.right)), X = () => /* @__PURE__ */ new Map(), E = Symbol("Equality"), re = (e, t) => {
  var n;
  return e === t || !!((n = e == null ? void 0 : e[E]) != null && n.call(e, t)) || !1;
}, ie = (e) => typeof e == "object", ae = Object.keys, gt = (e) => ae(e).length, L = (e, t) => {
  for (const n in e)
    if (!t(e[n], n))
      return !1;
  return !0;
}, Ct = (e, t) => Object.prototype.hasOwnProperty.call(e, t), lt = (e, t) => {
  for (let n = 0; n < e.length; n++)
    if (!t(e[n], n, e))
      return !1;
  return !0;
}, Tt = (e, t) => {
  for (let n = 0; n < e.length; n++)
    if (t(e[n], n, e))
      return !0;
  return !1;
}, le = (e, t) => {
  const n = new Array(e);
  for (let s = 0; s < e; s++)
    n[s] = t(s, n);
  return n;
}, ut = Array.isArray, U = (e) => new Error(e), ue = () => {
  throw U("Method unimplemented");
}, Ot = () => {
  throw U("Unexpected case");
}, he = String.fromCharCode, fe = (e) => e.toLowerCase(), de = /^\s*/g, pe = (e) => e.replace(de, ""), me = /([A-Z])/g, xt = (e, t) => pe(e.replace(me, (n) => `${t}${fe(n)}`));
typeof TextEncoder < "u" && new TextEncoder();
let K = typeof TextDecoder > "u" ? null : new TextDecoder("utf-8", { fatal: !0, ignoreBOM: !0 });
K && K.decode(new Uint8Array()).length === 1 && (K = null);
const ye = (e, t) => le(t, () => e).join(""), St = (e) => e === void 0 ? null : e;
class ge {
  constructor() {
    this.map = /* @__PURE__ */ new Map();
  }
  /**
   * @param {string} key
   * @param {any} newValue
   */
  setItem(t, n) {
    this.map.set(t, n);
  }
  /**
   * @param {string} key
   */
  getItem(t) {
    return this.map.get(t);
  }
}
let Mt = new ge(), xe = !0;
try {
  typeof localStorage < "u" && localStorage && (Mt = localStorage, xe = !1);
} catch {
}
const Se = Mt, P = (e, t) => {
  if (e === t)
    return !0;
  if (e == null || t == null || e.constructor !== t.constructor && (e.constructor || Object) !== (t.constructor || Object))
    return !1;
  if (e[E] != null)
    return e[E](t);
  switch (e.constructor) {
    case ArrayBuffer:
      e = new Uint8Array(e), t = new Uint8Array(t);
    // eslint-disable-next-line no-fallthrough
    case Uint8Array: {
      if (e.byteLength !== t.byteLength)
        return !1;
      for (let n = 0; n < e.length; n++)
        if (e[n] !== t[n])
          return !1;
      break;
    }
    case Set: {
      if (e.size !== t.size)
        return !1;
      for (const n of e)
        if (!t.has(n))
          return !1;
      break;
    }
    case Map: {
      if (e.size !== t.size)
        return !1;
      for (const n of e.keys())
        if (!t.has(n) || !P(e.get(n), t.get(n)))
          return !1;
      break;
    }
    case void 0:
    case Object:
      if (gt(e) !== gt(t))
        return !1;
      for (const n in e)
        if (!Ct(e, n) || !P(e[n], t[n]))
          return !1;
      break;
    case Array:
      if (e.length !== t.length)
        return !1;
      for (let n = 0; n < e.length; n++)
        if (!P(e[n], t[n]))
          return !1;
      break;
    default:
      return !1;
  }
  return !0;
}, _e = (e, t) => t.includes(e), w = typeof process < "u" && process.release && /node|io\.js/.test(process.release.name) && Object.prototype.toString.call(typeof process < "u" ? process : 0) === "[object process]";
let m;
const $e = () => {
  if (m === void 0)
    if (w) {
      m = X();
      const e = process.argv;
      let t = null;
      for (let n = 0; n < e.length; n++) {
        const s = e[n];
        s[0] === "-" ? (t !== null && m.set(t, ""), t = s) : t !== null && (m.set(t, s), t = null);
      }
      t !== null && m.set(t, "");
    } else typeof location == "object" ? (m = X(), (location.search || "?").slice(1).split("&").forEach((e) => {
      if (e.length !== 0) {
        const [t, n] = e.split("=");
        m.set(`--${xt(t, "-")}`, n), m.set(`-${xt(t, "-")}`, n);
      }
    })) : m = X();
  return m;
}, nt = (e) => $e().has(e), st = (e) => St(w ? process.env[e.toUpperCase().replaceAll("-", "_")] : Se.getItem(e)), Rt = (e) => nt("--" + e) || st(e) !== null, ke = Rt("production"), be = w && _e(process.env.FORCE_COLOR, ["true", "1", "2"]);
be || !nt("--no-colors") && // @todo deprecate --no-colors
!Rt("no-color") && (!w || process.stdout.isTTY) && (!w || nt("--color") || st("COLORTERM") !== null || (st("TERM") || "").includes("color"));
const Nt = Math.floor, Ee = (e, t) => e < t ? e : t, we = (e, t) => e > t ? e : t, _t = Number.MAX_SAFE_INTEGER, $t = Number.MIN_SAFE_INTEGER, kt = (e) => e.next() >= 0.5, Z = (e, t, n) => Nt(e.next() * (n + 1 - t) + t), Pt = (e, t, n) => Nt(e.next() * (n + 1 - t) + t), ht = (e, t, n) => Pt(e, t, n), Ce = (e) => he(ht(e, 97, 122)), Te = (e, t = 0, n = 20) => {
  const s = ht(e, t, n);
  let o = "";
  for (let c = 0; c < s; c++)
    o += Ce(e);
  return o;
}, Q = (e, t) => t[ht(e, 0, t.length - 1)], Oe = Symbol("0schema");
class Me {
  constructor() {
    this._rerrs = [];
  }
  /**
   * @param {string?} path
   * @param {string} expected
   * @param {string} has
   * @param {string?} message
   */
  extend(t, n, s, o = null) {
    this._rerrs.push({ path: t, expected: n, has: s, message: o });
  }
  toString() {
    const t = [];
    for (let n = this._rerrs.length - 1; n > 0; n--) {
      const s = this._rerrs[n];
      t.push(ye(" ", (this._rerrs.length - n) * 2) + `${s.path != null ? `[${s.path}] ` : ""}${s.has} doesn't match ${s.expected}. ${s.message}`);
    }
    return t.join(`
`);
  }
}
const ot = (e, t) => e === t ? !0 : e == null || t == null || e.constructor !== t.constructor ? !1 : e[E] ? re(e, t) : ut(e) ? lt(
  e,
  (n) => Tt(t, (s) => ot(n, s))
) : ie(e) ? L(
  e,
  (n, s) => ot(n, t[s])
) : !1;
class h {
  /**
   * @param {Schema<any>} other
   */
  extends(t) {
    let [n, s] = [
      /** @type {any} */
      this.shape,
      /** @type {any} */
      t.shape
    ];
    return (
      /** @type {typeof Schema<any>} */
      this.constructor._dilutes && ([s, n] = [n, s]), ot(n, s)
    );
  }
  /**
   * Overwrite this when necessary. By default, we only check the `shape` property which every shape
   * should have.
   * @param {Schema<any>} other
   */
  equals(t) {
    return this.constructor === t.constructor && P(this.shape, t.shape);
  }
  [Oe]() {
    return !0;
  }
  /**
   * @param {object} other
   */
  [E](t) {
    return this.equals(
      /** @type {any} */
      t
    );
  }
  /**
   * Use `schema.validate(obj)` with a typed parameter that is already of typed to be an instance of
   * Schema. Validate will check the structure of the parameter and return true iff the instance
   * really is an instance of Schema.
   *
   * @param {T} o
   * @return {boolean}
   */
  validate(t) {
    return this.check(t);
  }
  /* c8 ignore start */
  /**
   * Similar to validate, but this method accepts untyped parameters.
   *
   * @param {any} _o
   * @param {ValidationError} [_err]
   * @return {_o is T}
   */
  check(t, n) {
    ue();
  }
  /* c8 ignore stop */
  /**
   * @type {Schema<T?>}
   */
  get nullable() {
    return k(this, J);
  }
  /**
   * @type {$Optional<Schema<T>>}
   */
  get optional() {
    return new Dt(
      /** @type {Schema<T>} */
      this
    );
  }
  /**
   * Cast a variable to a specific type. Returns the casted value, or throws an exception otherwise.
   * Use this if you know that the type is of a specific type and you just want to convince the type
   * system.
   *
   * **Do not rely on these error messages!**
   * Performs an assertion check only if not in a production environment.
   *
   * @template OO
   * @param {OO} o
   * @return {Extract<OO, T> extends never ? T : (OO extends Array<never> ? T : Extract<OO,T>)}
   */
  cast(t) {
    return bt(t, this), /** @type {any} */
    t;
  }
  /**
   * EXPECTO PATRONUM!! 🪄
   * This function protects against type errors. Though it may not work in the real world.
   *
   * "After all this time?"
   * "Always." - Snape, talking about type safety
   *
   * Ensures that a variable is a a specific type. Returns the value, or throws an exception if the assertion check failed.
   * Use this if you know that the type is of a specific type and you just want to convince the type
   * system.
   *
   * Can be useful when defining lambdas: `s.lambda(s.$number, s.$void).expect((n) => n + 1)`
   *
   * **Do not rely on these error messages!**
   * Performs an assertion check if not in a production environment.
   *
   * @param {T} o
   * @return {o extends T ? T : never}
   */
  expect(t) {
    return bt(t, this), t;
  }
}
// this.shape must not be defined on Schema. Otherwise typecheck on metatypes (e.g. $$object) won't work as expected anymore
/**
 * If true, the more things are added to the shape the more objects this schema will accept (e.g.
 * union). By default, the more objects are added, the the fewer objects this schema will accept.
 * @protected
 */
M(h, "_dilutes", !1);
class ft extends h {
  /**
   * @param {C} c
   * @param {((o:Instance<C>)=>boolean)|null} check
   */
  constructor(t, n) {
    super(), this.shape = t, this._c = n;
  }
  /**
   * @param {any} o
   * @param {ValidationError} [err]
   * @return {o is C extends ((...args:any[]) => infer T) ? T : (C extends (new (...args:any[]) => any) ? InstanceType<C> : never)} o
   */
  check(t, n = void 0) {
    const s = (t == null ? void 0 : t.constructor) === this.shape && (this._c == null || this._c(t));
    return !s && (n == null || n.extend(null, this.shape.name, t == null ? void 0 : t.constructor.name, (t == null ? void 0 : t.constructor) !== this.shape ? "Constructor match failed" : "Check failed")), s;
  }
}
const i = (e, t = null) => new ft(e, t);
i(ft);
class dt extends h {
  /**
   * @param {(o:any) => boolean} check
   */
  constructor(t) {
    super(), this.shape = t;
  }
  /**
   * @param {any} o
   * @param {ValidationError} err
   * @return {o is any}
   */
  check(t, n) {
    const s = this.shape(t);
    return !s && (n == null || n.extend(null, "custom prop", t == null ? void 0 : t.constructor.name, "failed to check custom prop")), s;
  }
}
const a = (e) => new dt(e);
i(dt);
class j extends h {
  /**
   * @param {Array<T>} literals
   */
  constructor(t) {
    super(), this.shape = t;
  }
  /**
   *
   * @param {any} o
   * @param {ValidationError} [err]
   * @return {o is T}
   */
  check(t, n) {
    const s = this.shape.some((o) => o === t);
    return !s && (n == null || n.extend(null, this.shape.join(" | "), t.toString())), s;
  }
}
const Y = (...e) => new j(e), vt = i(j), Re = (
  /** @type {any} */
  RegExp.escape || /** @type {(str:string) => string} */
  ((e) => e.replace(/[().|&,$^[\]]/g, (t) => "\\" + t))
), At = (e) => {
  if ($.check(e))
    return [Re(e)];
  if (vt.check(e))
    return (
      /** @type {Array<string|number>} */
      e.shape.map((t) => t + "")
    );
  if (Jt.check(e))
    return ["[+-]?\\d+.?\\d*"];
  if (qt.check(e))
    return [".*"];
  if (A.check(e))
    return e.shape.map(At).flat(1);
  Ot();
};
class Ne extends h {
  /**
   * @param {T} shape
   */
  constructor(t) {
    super(), this.shape = t, this._r = new RegExp("^" + t.map(At).map((n) => `(${n.join("|")})`).join("") + "$");
  }
  /**
   * @param {any} o
   * @param {ValidationError} [err]
   * @return {o is CastStringTemplateArgsToTemplate<T>}
   */
  check(t, n) {
    const s = this._r.exec(t) != null;
    return !s && (n == null || n.extend(null, this._r.toString(), t.toString(), "String doesn't match string template.")), s;
  }
}
i(Ne);
const Pe = Symbol("optional");
class Dt extends h {
  /**
   * @param {S} shape
   */
  constructor(t) {
    super(), this.shape = t;
  }
  /**
   * @param {any} o
   * @param {ValidationError} [err]
   * @return {o is (Unwrap<S>|undefined)}
   */
  check(t, n) {
    const s = t === void 0 || this.shape.check(t);
    return !s && (n == null || n.extend(null, "undefined (optional)", "()")), s;
  }
  get [Pe]() {
    return !0;
  }
}
const ve = i(Dt);
class Ae extends h {
  /**
   * @param {any} _o
   * @param {ValidationError} [err]
   * @return {_o is never}
   */
  check(t, n) {
    return n == null || n.extend(null, "never", typeof t), !1;
  }
}
i(Ae);
const F = class F extends h {
  /**
   * @param {S} shape
   * @param {boolean} partial
   */
  constructor(t, n = !1) {
    super(), this.shape = t, this._isPartial = n;
  }
  /**
   * @type {Schema<Partial<$ObjectToType<S>>>}
   */
  get partial() {
    return new F(this.shape, !0);
  }
  /**
   * @param {any} o
   * @param {ValidationError} err
   * @return {o is $ObjectToType<S>}
   */
  check(t, n) {
    return t == null ? (n == null || n.extend(null, "object", "null"), !1) : L(this.shape, (s, o) => {
      const c = this._isPartial && !Ct(t, o) || s.check(t[o], n);
      return !c && (n == null || n.extend(o.toString(), s.toString(), typeof t[o], "Object property does not match")), c;
    });
  }
};
M(F, "_dilutes", !0);
let v = F;
const De = (e) => (
  /** @type {any} */
  new v(e)
), Fe = i(v), Ie = a((e) => e != null && (e.constructor === Object || e.constructor == null));
class Ft extends h {
  /**
   * @param {Keys} keys
   * @param {Values} values
   */
  constructor(t, n) {
    super(), this.shape = {
      keys: t,
      values: n
    };
  }
  /**
   * @param {any} o
   * @param {ValidationError} err
   * @return {o is { [key in Unwrap<Keys>]: Unwrap<Values> }}
   */
  check(t, n) {
    return t != null && L(t, (s, o) => {
      const c = this.shape.keys.check(o, n);
      return !c && (n == null || n.extend(o + "", "Record", typeof t, c ? "Key doesn't match schema" : "Value doesn't match value")), c && this.shape.values.check(s, n);
    });
  }
}
const It = (e, t) => new Ft(e, t), Le = i(Ft);
class Lt extends h {
  /**
   * @param {S} shape
   */
  constructor(t) {
    super(), this.shape = t;
  }
  /**
   * @param {any} o
   * @param {ValidationError} err
   * @return {o is { [K in keyof S]: S[K] extends Schema<infer Type> ? Type : never }}
   */
  check(t, n) {
    return t != null && L(this.shape, (s, o) => {
      const c = (
        /** @type {Schema<any>} */
        s.check(t[o], n)
      );
      return !c && (n == null || n.extend(o.toString(), "Tuple", typeof s)), c;
    });
  }
}
const Ue = (...e) => new Lt(e);
i(Lt);
class Ut extends h {
  /**
   * @param {Array<S>} v
   */
  constructor(t) {
    super(), this.shape = t.length === 1 ? t[0] : new z(t);
  }
  /**
   * @param {any} o
   * @param {ValidationError} [err]
   * @return {o is Array<S extends Schema<infer T> ? T : never>} o
   */
  check(t, n) {
    const s = ut(t) && lt(t, (o) => this.shape.check(o));
    return !s && (n == null || n.extend(null, "Array", "")), s;
  }
}
const jt = (...e) => new Ut(e), je = i(Ut), Ye = a((e) => ut(e));
class Yt extends h {
  /**
   * @param {new (...args:any) => T} constructor
   * @param {((o:T) => boolean)|null} check
   */
  constructor(t, n) {
    super(), this.shape = t, this._c = n;
  }
  /**
   * @param {any} o
   * @param {ValidationError} err
   * @return {o is T}
   */
  check(t, n) {
    const s = t instanceof this.shape && (this._c == null || this._c(t));
    return !s && (n == null || n.extend(null, this.shape.name, t == null ? void 0 : t.constructor.name)), s;
  }
}
const ze = (e, t = null) => new Yt(e, t);
i(Yt);
const Ve = ze(h);
class Je extends h {
  /**
   * @param {Args} args
   */
  constructor(t) {
    super(), this.len = t.length - 1, this.args = Ue(...t.slice(-1)), this.res = t[this.len];
  }
  /**
   * @param {any} f
   * @param {ValidationError} err
   * @return {f is _LArgsToLambdaDef<Args>}
   */
  check(t, n) {
    const s = t.constructor === Function && t.length <= this.len;
    return !s && (n == null || n.extend(null, "function", typeof t)), s;
  }
}
const qe = i(Je), Be = a((e) => typeof e == "function");
class Ge extends h {
  /**
   * @param {T} v
   */
  constructor(t) {
    super(), this.shape = t;
  }
  /**
   * @param {any} o
   * @param {ValidationError} [err]
   * @return {o is Intersect<UnwrapArray<T>>}
   */
  check(t, n) {
    const s = lt(this.shape, (o) => o.check(t, n));
    return !s && (n == null || n.extend(null, "Intersectinon", typeof t)), s;
  }
}
i(Ge, (e) => e.shape.length > 0);
class z extends h {
  /**
   * @param {Array<Schema<S>>} v
   */
  constructor(t) {
    super(), this.shape = t;
  }
  /**
   * @param {any} o
   * @param {ValidationError} [err]
   * @return {o is S}
   */
  check(t, n) {
    const s = Tt(this.shape, (o) => o.check(t, n));
    return n == null || n.extend(null, "Union", typeof t), s;
  }
}
M(z, "_dilutes", !0);
const k = (...e) => e.findIndex((t) => A.check(t)) >= 0 ? k(...e.map((t) => C(t)).map((t) => A.check(t) ? t.shape : [t]).flat(1)) : e.length === 1 ? e[0] : new z(e), A = (
  /** @type {Schema<$Union<any>>} */
  i(z)
), zt = () => !0, D = a(zt), He = (
  /** @type {Schema<Schema<any>>} */
  i(dt, (e) => e.shape === zt)
), pt = a((e) => typeof e == "bigint"), We = (
  /** @type {Schema<Schema<BigInt>>} */
  a((e) => e === pt)
), Vt = a((e) => typeof e == "symbol");
a((e) => e === Vt);
const _ = a((e) => typeof e == "number"), Jt = (
  /** @type {Schema<Schema<number>>} */
  a((e) => e === _)
), $ = a((e) => typeof e == "string"), qt = (
  /** @type {Schema<Schema<string>>} */
  a((e) => e === $)
), V = a((e) => typeof e == "boolean"), Xe = (
  /** @type {Schema<Schema<Boolean>>} */
  a((e) => e === V)
), Bt = Y(void 0);
i(j, (e) => e.shape.length === 1 && e.shape[0] === void 0);
Y(void 0);
const J = Y(null), Ke = (
  /** @type {Schema<Schema<null>>} */
  i(j, (e) => e.shape.length === 1 && e.shape[0] === null)
);
i(Uint8Array);
i(ft, (e) => e.shape === Uint8Array);
const Ze = k(_, $, J, Bt, pt, V, Vt);
(() => {
  const e = (
    /** @type {$Array<$any>} */
    jt(D)
  ), t = (
    /** @type {$Record<$string,$any>} */
    It($, D)
  ), n = k(_, $, J, V, e, t);
  return e.shape = n, t.shape.values = n, n;
})();
const C = (e) => {
  if (Ve.check(e))
    return (
      /** @type {any} */
      e
    );
  if (Ie.check(e)) {
    const t = {};
    for (const n in e)
      t[n] = C(e[n]);
    return (
      /** @type {any} */
      De(t)
    );
  } else {
    if (Ye.check(e))
      return (
        /** @type {any} */
        k(...e.map(C))
      );
    if (Ze.check(e))
      return (
        /** @type {any} */
        Y(e)
      );
    if (Be.check(e))
      return (
        /** @type {any} */
        i(
          /** @type {any} */
          e
        )
      );
  }
  Ot();
}, bt = ke ? () => {
} : (e, t) => {
  const n = new Me();
  if (!t.check(e, n))
    throw U(`Expected value to be of type ${t.constructor.name}.
${n.toString()}`);
};
class Qe {
  /**
   * @param {Schema<State>} [$state]
   */
  constructor(t) {
    this.patterns = [], this.$state = t;
  }
  /**
   * @template P
   * @template R
   * @param {P} pattern
   * @param {(o:NoInfer<Unwrap<ReadSchema<P>>>,s:State)=>R} handler
   * @return {PatternMatcher<State,Patterns|Pattern<Unwrap<ReadSchema<P>>,R>>}
   */
  if(t, n) {
    return this.patterns.push({ if: C(t), h: n }), this;
  }
  /**
   * @template R
   * @param {(o:any,s:State)=>R} h
   */
  else(t) {
    return this.if(D, t);
  }
  /**
   * @return {State extends undefined
   *   ? <In extends Unwrap<Patterns['if']>>(o:In,state?:undefined)=>PatternMatchResult<Patterns,In>
   *   : <In extends Unwrap<Patterns['if']>>(o:In,state:State)=>PatternMatchResult<Patterns,In>}
   */
  done() {
    return (
      /** @type {any} */
      (t, n) => {
        for (let s = 0; s < this.patterns.length; s++) {
          const o = this.patterns[s];
          if (o.if.check(t))
            return o.h(t, n);
        }
        throw U("Unhandled pattern");
      }
    );
  }
}
const tn = (e) => new Qe(
  /** @type {any} */
  e
), Gt = (
  /** @type {any} */
  tn(
    /** @type {Schema<prng.PRNG>} */
    D
  ).if(Jt, (e, t) => Z(t, $t, _t)).if(qt, (e, t) => Te(t)).if(Xe, (e, t) => kt(t)).if(We, (e, t) => BigInt(Z(t, $t, _t))).if(A, (e, t) => S(t, Q(t, e.shape))).if(Fe, (e, t) => {
    const n = {};
    for (const s in e.shape) {
      let o = e.shape[s];
      if (ve.check(o)) {
        if (kt(t))
          continue;
        o = o.shape;
      }
      n[s] = Gt(o, t);
    }
    return n;
  }).if(je, (e, t) => {
    const n = [], s = Pt(t, 0, 42);
    for (let o = 0; o < s; o++)
      n.push(S(t, e.shape));
    return n;
  }).if(vt, (e, t) => Q(t, e.shape)).if(Ke, (e, t) => null).if(qe, (e, t) => {
    const n = S(t, e.res);
    return () => n;
  }).if(He, (e, t) => S(t, Q(t, [
    _,
    $,
    J,
    Bt,
    pt,
    V,
    jt(_),
    It(k("a", "b", "c"), _)
  ]))).if(Le, (e, t) => {
    const n = {}, s = Z(t, 0, 3);
    for (let o = 0; o < s; o++) {
      const c = S(t, e.shape.keys), r = S(t, e.shape.values);
      n[c] = r;
    }
    return n;
  }).done()
), S = (e, t) => (
  /** @type {any} */
  Gt(C(t), e)
), y = (
  /** @type {Document} */
  typeof document < "u" ? document : {}
), en = (e) => y.createElement(e), nn = () => y.createDocumentFragment();
a((e) => e.nodeType === hn);
const sn = (e) => y.createTextNode(e);
typeof DOMParser < "u" && new DOMParser();
const on = (e, t) => (ce(t, (n, s) => {
  s === !1 ? e.removeAttribute(n) : s === !0 ? e.setAttribute(n, "") : e.setAttribute(n, s);
}), e), cn = (e) => {
  const t = nn();
  for (let n = 0; n < e.length; n++)
    Ht(t, e[n]);
  return t;
}, rn = (e, t) => (Ht(e, cn(t)), e), tt = (e, t = [], n = []) => rn(on(en(e), t), n);
a((e) => e.nodeType === an);
const N = sn;
a((e) => e.nodeType === ln);
const Ht = (e, t) => e.appendChild(t), an = y.ELEMENT_NODE, ln = y.TEXT_NODE;
y.CDATA_SECTION_NODE;
y.COMMENT_NODE;
const un = y.DOCUMENT_NODE;
y.DOCUMENT_TYPE_NODE;
const hn = y.DOCUMENT_FRAGMENT_NODE;
a((e) => e.nodeType === un);
const fn = it.baseTheme({
  ".cm-ySelection": {},
  ".cm-yLineSelection": {
    padding: 0,
    margin: "0px 2px 0px 4px"
  },
  ".cm-ySelectionCaret": {
    position: "relative",
    borderLeft: "1px solid black",
    borderRight: "1px solid black",
    marginLeft: "-1px",
    marginRight: "-1px",
    boxSizing: "border-box",
    display: "inline"
  },
  ".cm-ySelectionCaretDot": {
    borderRadius: "50%",
    position: "absolute",
    width: ".4em",
    height: ".4em",
    top: "-.2em",
    left: "-.2em",
    backgroundColor: "inherit",
    transition: "transform .3s ease-in-out",
    boxSizing: "border-box"
  },
  ".cm-ySelectionCaret:hover > .cm-ySelectionCaretDot": {
    transformOrigin: "bottom center",
    transform: "scale(0)"
  },
  ".cm-ySelectionInfo": {
    position: "absolute",
    top: "-1.05em",
    left: "-1px",
    fontSize: ".75em",
    fontFamily: "serif",
    fontStyle: "normal",
    fontWeight: "normal",
    lineHeight: "normal",
    userSelect: "none",
    color: "white",
    paddingLeft: "2px",
    paddingRight: "2px",
    zIndex: 101,
    transition: "opacity .3s ease-in-out",
    backgroundColor: "inherit",
    // these should be separate
    opacity: 0,
    transitionDelay: "0s",
    whiteSpace: "nowrap"
  },
  ".cm-ySelectionCaret:hover > .cm-ySelectionInfo": {
    opacity: 1,
    transitionDelay: "0s"
  }
}), dn = wt.define();
class pn extends te {
  /**
   * @param {string} color
   * @param {string} name
   */
  constructor(t, n) {
    super(), this.color = t, this.name = n;
  }
  toDOM() {
    return (
      /** @type {HTMLElement} */
      tt("span", [R("class", "cm-ySelectionCaret"), R("style", `background-color: ${this.color}; border-color: ${this.color}`)], [
        N("⁠"),
        tt("div", [
          R("class", "cm-ySelectionCaretDot")
        ]),
        N("⁠"),
        tt("div", [
          R("class", "cm-ySelectionInfo")
        ], [
          N(this.name)
        ]),
        N("⁠")
      ])
    );
  }
  eq(t) {
    return t.color === this.color;
  }
  compare(t) {
    return t.color === this.color;
  }
  updateDOM() {
    return !1;
  }
  get estimatedHeight() {
    return -1;
  }
  ignoreEvent() {
    return !0;
  }
}
class mn {
  /**
   * @param {cmView.EditorView} view
   */
  constructor(t) {
    this.conf = t.state.facet(I), this._listener = ({ added: n, updated: s, removed: o }, c, r) => {
      n.concat(s).concat(o).findIndex((l) => l !== this.conf.awareness.doc.clientID) >= 0 && t.dispatch({ annotations: [dn.of([])] });
    }, this._awareness = this.conf.awareness, this._awareness.on("change", this._listener), this.decorations = Qt.of([]);
  }
  destroy() {
    this._awareness.off("change", this._listener);
  }
  /**
   * @param {cmView.ViewUpdate} update
   */
  update(t) {
    const n = this.conf.ytext, s = (
      /** @type {Y.Doc} */
      n.doc
    ), o = this.conf.awareness, c = [], r = this.conf.awareness.getLocalState();
    if (r != null) {
      const f = t.view.hasFocus && t.view.dom.ownerDocument.hasFocus(), l = f ? t.state.selection.main : null, d = r.cursor == null ? null : u.createRelativePositionFromJSON(r.cursor.anchor), g = r.cursor == null ? null : u.createRelativePositionFromJSON(r.cursor.head);
      if (l != null) {
        const p = u.createRelativePositionFromTypeIndex(n, l.anchor), b = u.createRelativePositionFromTypeIndex(n, l.head);
        (r.cursor == null || !u.compareRelativePositions(d, p) || !u.compareRelativePositions(g, b)) && o.setLocalStateField("cursor", {
          anchor: p,
          head: b
        });
      } else r.cursor != null && f && o.setLocalStateField("cursor", null);
    }
    o.getStates().forEach((f, l) => {
      if (l === o.doc.clientID)
        return;
      const d = f.cursor;
      if (d == null || d.anchor == null || d.head == null)
        return;
      const g = u.createAbsolutePositionFromRelativePosition(d.anchor, s), p = u.createAbsolutePositionFromRelativePosition(d.head, s);
      if (g == null || p == null || g.type !== n || p.type !== n)
        return;
      const { color: b = "#30bced", name: Xt = "Anonymous" } = f.user || {}, T = f.user && f.user.colorLight || b + "33", B = Ee(g.index, p.index), G = we(g.index, p.index), O = t.view.state.doc.lineAt(B), H = t.view.state.doc.lineAt(G);
      if (O.number === H.number)
        c.push({
          from: B,
          to: G,
          value: x.mark({
            attributes: { style: `background-color: ${T}` },
            class: "cm-ySelection"
          })
        });
      else {
        c.push({
          from: B,
          to: O.from + O.length,
          value: x.mark({
            attributes: { style: `background-color: ${T}` },
            class: "cm-ySelection"
          })
        }), c.push({
          from: H.from,
          to: G,
          value: x.mark({
            attributes: { style: `background-color: ${T}` },
            class: "cm-ySelection"
          })
        });
        for (let W = O.number + 1; W < H.number; W++) {
          const mt = t.view.state.doc.line(W).from;
          c.push({
            from: mt,
            to: mt,
            value: x.line({
              attributes: { style: `background-color: ${T}`, class: "cm-yLineSelection" }
            })
          });
        }
      }
      c.push({
        from: p.index,
        to: p.index,
        value: x.widget({
          side: p.index - g.index > 0 ? -1 : 1,
          // the local cursor should be rendered outside the remote selection
          block: !1,
          widget: new pn(b, Xt)
        })
      });
    }), this.decorations = x.set(c, !0);
  }
}
const yn = rt.fromClass(mn, {
  decorations: (e) => e.decorations
}), gn = () => {
  let e = !0;
  return (t, n) => {
    if (e) {
      e = !1;
      try {
        t();
      } finally {
        e = !0;
      }
    } else n !== void 0 && n();
  };
};
class xn {
  /**
   * @param {Y.UndoManager} undoManager
   */
  constructor(t) {
    this.undoManager = t;
  }
  /**
   * @param {any} origin
   */
  addTrackedOrigin(t) {
    this.undoManager.addTrackedOrigin(t);
  }
  /**
   * @param {any} origin
   */
  removeTrackedOrigin(t) {
    this.undoManager.removeTrackedOrigin(t);
  }
  /**
   * @return {boolean} Whether a change was undone.
   */
  undo() {
    return this.undoManager.undo() != null;
  }
  /**
   * @return {boolean} Whether a change was redone.
   */
  redo() {
    return this.undoManager.redo() != null;
  }
}
const q = Et.define({
  combine(e) {
    return e[e.length - 1];
  }
});
class Sn {
  /**
   * @param {cmView.EditorView} view
   */
  constructor(t) {
    this.view = t, this.conf = t.state.facet(q), this._undoManager = this.conf.undoManager, this.syncConf = t.state.facet(I), this._beforeChangeSelection = null, this._mux = gn(), this._onStackItemAdded = ({ stackItem: n, changedParentTypes: s }) => {
      s.has(this.syncConf.ytext) && this._beforeChangeSelection && !n.meta.has(this) && n.meta.set(this, this._beforeChangeSelection);
    }, this._onStackItemPopped = ({ stackItem: n }) => {
      const s = n.meta.get(this);
      if (s) {
        const o = this.syncConf.fromYRange(s);
        t.dispatch(t.state.update({
          selection: o,
          effects: [it.scrollIntoView(o)]
        })), this._storeSelection();
      }
    }, this._storeSelection = () => {
      this._beforeChangeSelection = this.syncConf.toYRange(this.view.state.selection.main);
    }, this._undoManager.on("stack-item-added", this._onStackItemAdded), this._undoManager.on("stack-item-popped", this._onStackItemPopped), this._undoManager.addTrackedOrigin(this.syncConf);
  }
  /**
   * @param {cmView.ViewUpdate} update
   */
  update(t) {
    t.selectionSet && (t.transactions.length === 0 || t.transactions[0].annotation(et) !== this.syncConf) && this._storeSelection();
  }
  destroy() {
    this._undoManager.off("stack-item-added", this._onStackItemAdded), this._undoManager.off("stack-item-popped", this._onStackItemPopped), this._undoManager.removeTrackedOrigin(this.syncConf);
  }
}
const _n = rt.fromClass(Sn), Wt = ({ state: e, dispatch: t }) => e.facet(q).undo() || !0, ct = ({ state: e, dispatch: t }) => e.facet(q).redo() || !0, bn = [
  { key: "Mod-z", run: Wt, preventDefault: !0 },
  { key: "Mod-y", mac: "Mod-Shift-z", run: ct, preventDefault: !0 },
  { key: "Mod-Shift-z", run: ct, preventDefault: !0 }
], En = (e, t, { undoManager: n = new u.UndoManager(e) } = {}) => {
  const s = new ee(e, t), o = [
    I.of(s),
    se
  ];
  return t && o.push(
    fn,
    yn
  ), n !== !1 && o.push(
    q.of(new xn(n)),
    _n,
    it.domEventHandlers({
      beforeinput(c, r) {
        return c.inputType === "historyUndo" ? Wt(r) : c.inputType === "historyRedo" ? ct(r) : !1;
      }
    })
  ), o;
};
export {
  at as YRange,
  ee as YSyncConfig,
  En as yCollab,
  yn as yRemoteSelections,
  fn as yRemoteSelectionsTheme,
  se as ySync,
  I as ySyncFacet,
  bn as yUndoManagerKeymap
};
//# sourceMappingURL=index-Xv-DOIsY.js.map
