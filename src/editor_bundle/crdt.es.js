import * as Y from "yjs";
const floor = Math.floor;
const abs = Math.abs;
const min = (a, b) => a < b ? a : b;
const max = (a, b) => a > b ? a : b;
const isNegativeZero = (n) => n !== 0 ? n < 0 : 1 / n < 0;
const BIT7 = 64;
const BIT8 = 128;
const BITS6 = 63;
const BITS7 = 127;
const BITS8 = 255;
const BITS31 = 2147483647;
const MAX_SAFE_INTEGER = Number.MAX_SAFE_INTEGER;
const isInteger = Number.isInteger || ((num) => typeof num === "number" && isFinite(num) && floor(num) === num);
const create$2 = () => /* @__PURE__ */ new Set();
const from = Array.from;
const isArray = Array.isArray;
const _encodeUtf8Polyfill = (str) => {
  const encodedString = unescape(encodeURIComponent(str));
  const len = encodedString.length;
  const buf = new Uint8Array(len);
  for (let i = 0; i < len; i++) {
    buf[i] = /** @type {number} */
    encodedString.codePointAt(i);
  }
  return buf;
};
const utf8TextEncoder = (
  /** @type {TextEncoder} */
  typeof TextEncoder !== "undefined" ? new TextEncoder() : null
);
const _encodeUtf8Native = (str) => utf8TextEncoder.encode(str);
const encodeUtf8 = utf8TextEncoder ? _encodeUtf8Native : _encodeUtf8Polyfill;
const _decodeUtf8Polyfill = (buf) => {
  let remainingLen = buf.length;
  let encodedString = "";
  let bufPos = 0;
  while (remainingLen > 0) {
    const nextLen = remainingLen < 1e4 ? remainingLen : 1e4;
    const bytes = buf.subarray(bufPos, bufPos + nextLen);
    bufPos += nextLen;
    encodedString += String.fromCodePoint.apply(
      null,
      /** @type {any} */
      bytes
    );
    remainingLen -= nextLen;
  }
  return decodeURIComponent(escape(encodedString));
};
let utf8TextDecoder = typeof TextDecoder === "undefined" ? null : new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
if (utf8TextDecoder && utf8TextDecoder.decode(new Uint8Array()).length === 1) {
  utf8TextDecoder = null;
}
const _decodeUtf8Native = (buf) => (
  /** @type {TextDecoder} */
  utf8TextDecoder.decode(buf)
);
const decodeUtf8 = utf8TextDecoder ? _decodeUtf8Native : _decodeUtf8Polyfill;
class Encoder {
  constructor() {
    this.cpos = 0;
    this.cbuf = new Uint8Array(100);
    this.bufs = [];
  }
}
const createEncoder = () => new Encoder();
const encode = (f) => {
  const encoder = createEncoder();
  f(encoder);
  return toUint8Array(encoder);
};
const length = (encoder) => {
  let len = encoder.cpos;
  for (let i = 0; i < encoder.bufs.length; i++) {
    len += encoder.bufs[i].length;
  }
  return len;
};
const hasContent$1 = (encoder) => encoder.cpos > 0 || encoder.bufs.length > 0;
const toUint8Array = (encoder) => {
  const uint8arr = new Uint8Array(length(encoder));
  let curPos = 0;
  for (let i = 0; i < encoder.bufs.length; i++) {
    const d = encoder.bufs[i];
    uint8arr.set(d, curPos);
    curPos += d.length;
  }
  uint8arr.set(new Uint8Array(encoder.cbuf.buffer, 0, encoder.cpos), curPos);
  return uint8arr;
};
const verifyLen = (encoder, len) => {
  const bufferLen = encoder.cbuf.length;
  if (bufferLen - encoder.cpos < len) {
    encoder.bufs.push(new Uint8Array(encoder.cbuf.buffer, 0, encoder.cpos));
    encoder.cbuf = new Uint8Array(max(bufferLen, len) * 2);
    encoder.cpos = 0;
  }
};
const write = (encoder, num) => {
  const bufferLen = encoder.cbuf.length;
  if (encoder.cpos === bufferLen) {
    encoder.bufs.push(encoder.cbuf);
    encoder.cbuf = new Uint8Array(bufferLen * 2);
    encoder.cpos = 0;
  }
  encoder.cbuf[encoder.cpos++] = num;
};
const set = (encoder, pos, num) => {
  let buffer = null;
  for (let i = 0; i < encoder.bufs.length && buffer === null; i++) {
    const b = encoder.bufs[i];
    if (pos < b.length) {
      buffer = b;
    } else {
      pos -= b.length;
    }
  }
  if (buffer === null) {
    buffer = encoder.cbuf;
  }
  buffer[pos] = num;
};
const writeUint8 = write;
const setUint8 = set;
const writeUint16 = (encoder, num) => {
  write(encoder, num & BITS8);
  write(encoder, num >>> 8 & BITS8);
};
const setUint16 = (encoder, pos, num) => {
  set(encoder, pos, num & BITS8);
  set(encoder, pos + 1, num >>> 8 & BITS8);
};
const writeUint32 = (encoder, num) => {
  for (let i = 0; i < 4; i++) {
    write(encoder, num & BITS8);
    num >>>= 8;
  }
};
const writeUint32BigEndian = (encoder, num) => {
  for (let i = 3; i >= 0; i--) {
    write(encoder, num >>> 8 * i & BITS8);
  }
};
const setUint32 = (encoder, pos, num) => {
  for (let i = 0; i < 4; i++) {
    set(encoder, pos + i, num & BITS8);
    num >>>= 8;
  }
};
const writeVarUint = (encoder, num) => {
  while (num > BITS7) {
    write(encoder, BIT8 | BITS7 & num);
    num = floor(num / 128);
  }
  write(encoder, BITS7 & num);
};
const writeVarInt = (encoder, num) => {
  const isNegative = isNegativeZero(num);
  if (isNegative) {
    num = -num;
  }
  write(encoder, (num > BITS6 ? BIT8 : 0) | (isNegative ? BIT7 : 0) | BITS6 & num);
  num = floor(num / 64);
  while (num > 0) {
    write(encoder, (num > BITS7 ? BIT8 : 0) | BITS7 & num);
    num = floor(num / 128);
  }
};
const _strBuffer = new Uint8Array(3e4);
const _maxStrBSize = _strBuffer.length / 3;
const _writeVarStringNative = (encoder, str) => {
  if (str.length < _maxStrBSize) {
    const written = utf8TextEncoder.encodeInto(str, _strBuffer).written || 0;
    writeVarUint(encoder, written);
    for (let i = 0; i < written; i++) {
      write(encoder, _strBuffer[i]);
    }
  } else {
    writeVarUint8Array(encoder, encodeUtf8(str));
  }
};
const _writeVarStringPolyfill = (encoder, str) => {
  const encodedString = unescape(encodeURIComponent(str));
  const len = encodedString.length;
  writeVarUint(encoder, len);
  for (let i = 0; i < len; i++) {
    write(
      encoder,
      /** @type {number} */
      encodedString.codePointAt(i)
    );
  }
};
const writeVarString = utf8TextEncoder && /** @type {any} */
utf8TextEncoder.encodeInto ? _writeVarStringNative : _writeVarStringPolyfill;
const writeTerminatedString = (encoder, str) => writeTerminatedUint8Array(encoder, encodeUtf8(str));
const writeTerminatedUint8Array = (encoder, buf) => {
  for (let i = 0; i < buf.length; i++) {
    const b = buf[i];
    if (b === 0 || b === 1) {
      write(encoder, 1);
    }
    write(encoder, buf[i]);
  }
  write(encoder, 0);
};
const writeBinaryEncoder = (encoder, append) => writeUint8Array(encoder, toUint8Array(append));
const writeUint8Array = (encoder, uint8Array) => {
  const bufferLen = encoder.cbuf.length;
  const cpos = encoder.cpos;
  const leftCopyLen = min(bufferLen - cpos, uint8Array.length);
  const rightCopyLen = uint8Array.length - leftCopyLen;
  encoder.cbuf.set(uint8Array.subarray(0, leftCopyLen), cpos);
  encoder.cpos += leftCopyLen;
  if (rightCopyLen > 0) {
    encoder.bufs.push(encoder.cbuf);
    encoder.cbuf = new Uint8Array(max(bufferLen * 2, rightCopyLen));
    encoder.cbuf.set(uint8Array.subarray(leftCopyLen));
    encoder.cpos = rightCopyLen;
  }
};
const writeVarUint8Array = (encoder, uint8Array) => {
  writeVarUint(encoder, uint8Array.byteLength);
  writeUint8Array(encoder, uint8Array);
};
const writeOnDataView = (encoder, len) => {
  verifyLen(encoder, len);
  const dview = new DataView(encoder.cbuf.buffer, encoder.cpos, len);
  encoder.cpos += len;
  return dview;
};
const writeFloat32 = (encoder, num) => writeOnDataView(encoder, 4).setFloat32(0, num, false);
const writeFloat64 = (encoder, num) => writeOnDataView(encoder, 8).setFloat64(0, num, false);
const writeBigInt64 = (encoder, num) => (
  /** @type {any} */
  writeOnDataView(encoder, 8).setBigInt64(0, num, false)
);
const writeBigUint64 = (encoder, num) => (
  /** @type {any} */
  writeOnDataView(encoder, 8).setBigUint64(0, num, false)
);
const floatTestBed = new DataView(new ArrayBuffer(4));
const isFloat32 = (num) => {
  floatTestBed.setFloat32(0, num);
  return floatTestBed.getFloat32(0) === num;
};
const writeAny = (encoder, data) => {
  switch (typeof data) {
    case "string":
      write(encoder, 119);
      writeVarString(encoder, data);
      break;
    case "number":
      if (isInteger(data) && abs(data) <= BITS31) {
        write(encoder, 125);
        writeVarInt(encoder, data);
      } else if (isFloat32(data)) {
        write(encoder, 124);
        writeFloat32(encoder, data);
      } else {
        write(encoder, 123);
        writeFloat64(encoder, data);
      }
      break;
    case "bigint":
      write(encoder, 122);
      writeBigInt64(encoder, data);
      break;
    case "object":
      if (data === null) {
        write(encoder, 126);
      } else if (isArray(data)) {
        write(encoder, 117);
        writeVarUint(encoder, data.length);
        for (let i = 0; i < data.length; i++) {
          writeAny(encoder, data[i]);
        }
      } else if (data instanceof Uint8Array) {
        write(encoder, 116);
        writeVarUint8Array(encoder, data);
      } else {
        write(encoder, 118);
        const keys2 = Object.keys(data);
        writeVarUint(encoder, keys2.length);
        for (let i = 0; i < keys2.length; i++) {
          const key = keys2[i];
          writeVarString(encoder, key);
          writeAny(encoder, data[key]);
        }
      }
      break;
    case "boolean":
      write(encoder, data ? 120 : 121);
      break;
    default:
      write(encoder, 127);
  }
};
class RleEncoder extends Encoder {
  /**
   * @param {function(Encoder, T):void} writer
   */
  constructor(writer) {
    super();
    this.w = writer;
    this.s = null;
    this.count = 0;
  }
  /**
   * @param {T} v
   */
  write(v) {
    if (this.s === v) {
      this.count++;
    } else {
      if (this.count > 0) {
        writeVarUint(this, this.count - 1);
      }
      this.count = 1;
      this.w(this, v);
      this.s = v;
    }
  }
}
class IntDiffEncoder extends Encoder {
  /**
   * @param {number} start
   */
  constructor(start) {
    super();
    this.s = start;
  }
  /**
   * @param {number} v
   */
  write(v) {
    writeVarInt(this, v - this.s);
    this.s = v;
  }
}
class RleIntDiffEncoder extends Encoder {
  /**
   * @param {number} start
   */
  constructor(start) {
    super();
    this.s = start;
    this.count = 0;
  }
  /**
   * @param {number} v
   */
  write(v) {
    if (this.s === v && this.count > 0) {
      this.count++;
    } else {
      if (this.count > 0) {
        writeVarUint(this, this.count - 1);
      }
      this.count = 1;
      writeVarInt(this, v - this.s);
      this.s = v;
    }
  }
}
const flushUintOptRleEncoder = (encoder) => {
  if (encoder.count > 0) {
    writeVarInt(encoder.encoder, encoder.count === 1 ? encoder.s : -encoder.s);
    if (encoder.count > 1) {
      writeVarUint(encoder.encoder, encoder.count - 2);
    }
  }
};
class UintOptRleEncoder {
  constructor() {
    this.encoder = new Encoder();
    this.s = 0;
    this.count = 0;
  }
  /**
   * @param {number} v
   */
  write(v) {
    if (this.s === v) {
      this.count++;
    } else {
      flushUintOptRleEncoder(this);
      this.count = 1;
      this.s = v;
    }
  }
  /**
   * Flush the encoded state and transform this to a Uint8Array.
   *
   * Note that this should only be called once.
   */
  toUint8Array() {
    flushUintOptRleEncoder(this);
    return toUint8Array(this.encoder);
  }
}
class IncUintOptRleEncoder {
  constructor() {
    this.encoder = new Encoder();
    this.s = 0;
    this.count = 0;
  }
  /**
   * @param {number} v
   */
  write(v) {
    if (this.s + this.count === v) {
      this.count++;
    } else {
      flushUintOptRleEncoder(this);
      this.count = 1;
      this.s = v;
    }
  }
  /**
   * Flush the encoded state and transform this to a Uint8Array.
   *
   * Note that this should only be called once.
   */
  toUint8Array() {
    flushUintOptRleEncoder(this);
    return toUint8Array(this.encoder);
  }
}
const flushIntDiffOptRleEncoder = (encoder) => {
  if (encoder.count > 0) {
    const encodedDiff = encoder.diff * 2 + (encoder.count === 1 ? 0 : 1);
    writeVarInt(encoder.encoder, encodedDiff);
    if (encoder.count > 1) {
      writeVarUint(encoder.encoder, encoder.count - 2);
    }
  }
};
class IntDiffOptRleEncoder {
  constructor() {
    this.encoder = new Encoder();
    this.s = 0;
    this.count = 0;
    this.diff = 0;
  }
  /**
   * @param {number} v
   */
  write(v) {
    if (this.diff === v - this.s) {
      this.s = v;
      this.count++;
    } else {
      flushIntDiffOptRleEncoder(this);
      this.count = 1;
      this.diff = v - this.s;
      this.s = v;
    }
  }
  /**
   * Flush the encoded state and transform this to a Uint8Array.
   *
   * Note that this should only be called once.
   */
  toUint8Array() {
    flushIntDiffOptRleEncoder(this);
    return toUint8Array(this.encoder);
  }
}
class StringEncoder {
  constructor() {
    this.sarr = [];
    this.s = "";
    this.lensE = new UintOptRleEncoder();
  }
  /**
   * @param {string} string
   */
  write(string) {
    this.s += string;
    if (this.s.length > 19) {
      this.sarr.push(this.s);
      this.s = "";
    }
    this.lensE.write(string.length);
  }
  toUint8Array() {
    const encoder = new Encoder();
    this.sarr.push(this.s);
    this.s = "";
    writeVarString(encoder, this.sarr.join(""));
    writeUint8Array(encoder, this.lensE.toUint8Array());
    return toUint8Array(encoder);
  }
}
const encoding = /* @__PURE__ */ Object.freeze(/* @__PURE__ */ Object.defineProperty({
  __proto__: null,
  Encoder,
  IncUintOptRleEncoder,
  IntDiffEncoder,
  IntDiffOptRleEncoder,
  RleEncoder,
  RleIntDiffEncoder,
  StringEncoder,
  UintOptRleEncoder,
  _writeVarStringNative,
  _writeVarStringPolyfill,
  createEncoder,
  encode,
  hasContent: hasContent$1,
  length,
  set,
  setUint16,
  setUint32,
  setUint8,
  toUint8Array,
  verifyLen,
  write,
  writeAny,
  writeBigInt64,
  writeBigUint64,
  writeBinaryEncoder,
  writeFloat32,
  writeFloat64,
  writeOnDataView,
  writeTerminatedString,
  writeTerminatedUint8Array,
  writeUint16,
  writeUint32,
  writeUint32BigEndian,
  writeUint8,
  writeUint8Array,
  writeVarInt,
  writeVarString,
  writeVarUint,
  writeVarUint8Array
}, Symbol.toStringTag, { value: "Module" }));
const create$1 = (s) => new Error(s);
const errorUnexpectedEndOfArray = create$1("Unexpected end of array");
const errorIntegerOutOfRange = create$1("Integer out of Range");
class Decoder {
  /**
   * @param {Uint8Array<Buf>} uint8Array Binary data to decode
   */
  constructor(uint8Array) {
    this.arr = uint8Array;
    this.pos = 0;
  }
}
const createDecoder = (uint8Array) => new Decoder(uint8Array);
const hasContent = (decoder) => decoder.pos !== decoder.arr.length;
const clone = (decoder, newPos = decoder.pos) => {
  const _decoder = createDecoder(decoder.arr);
  _decoder.pos = newPos;
  return _decoder;
};
const readUint8Array = (decoder, len) => {
  const view = new Uint8Array(decoder.arr.buffer, decoder.pos + decoder.arr.byteOffset, len);
  decoder.pos += len;
  return view;
};
const readVarUint8Array = (decoder) => readUint8Array(decoder, readVarUint(decoder));
const readTailAsUint8Array = (decoder) => readUint8Array(decoder, decoder.arr.length - decoder.pos);
const skip8 = (decoder) => decoder.pos++;
const readUint8 = (decoder) => decoder.arr[decoder.pos++];
const readUint16 = (decoder) => {
  const uint = decoder.arr[decoder.pos] + (decoder.arr[decoder.pos + 1] << 8);
  decoder.pos += 2;
  return uint;
};
const readUint32 = (decoder) => {
  const uint = decoder.arr[decoder.pos] + (decoder.arr[decoder.pos + 1] << 8) + (decoder.arr[decoder.pos + 2] << 16) + (decoder.arr[decoder.pos + 3] << 24) >>> 0;
  decoder.pos += 4;
  return uint;
};
const readUint32BigEndian = (decoder) => {
  const uint = decoder.arr[decoder.pos + 3] + (decoder.arr[decoder.pos + 2] << 8) + (decoder.arr[decoder.pos + 1] << 16) + (decoder.arr[decoder.pos] << 24) >>> 0;
  decoder.pos += 4;
  return uint;
};
const peekUint8 = (decoder) => decoder.arr[decoder.pos];
const peekUint16 = (decoder) => decoder.arr[decoder.pos] + (decoder.arr[decoder.pos + 1] << 8);
const peekUint32 = (decoder) => decoder.arr[decoder.pos] + (decoder.arr[decoder.pos + 1] << 8) + (decoder.arr[decoder.pos + 2] << 16) + (decoder.arr[decoder.pos + 3] << 24) >>> 0;
const readVarUint = (decoder) => {
  let num = 0;
  let mult = 1;
  const len = decoder.arr.length;
  while (decoder.pos < len) {
    const r = decoder.arr[decoder.pos++];
    num = num + (r & BITS7) * mult;
    mult *= 128;
    if (r < BIT8) {
      return num;
    }
    if (num > MAX_SAFE_INTEGER) {
      throw errorIntegerOutOfRange;
    }
  }
  throw errorUnexpectedEndOfArray;
};
const readVarInt = (decoder) => {
  let r = decoder.arr[decoder.pos++];
  let num = r & BITS6;
  let mult = 64;
  const sign = (r & BIT7) > 0 ? -1 : 1;
  if ((r & BIT8) === 0) {
    return sign * num;
  }
  const len = decoder.arr.length;
  while (decoder.pos < len) {
    r = decoder.arr[decoder.pos++];
    num = num + (r & BITS7) * mult;
    mult *= 128;
    if (r < BIT8) {
      return sign * num;
    }
    if (num > MAX_SAFE_INTEGER) {
      throw errorIntegerOutOfRange;
    }
  }
  throw errorUnexpectedEndOfArray;
};
const peekVarUint = (decoder) => {
  const pos = decoder.pos;
  const s = readVarUint(decoder);
  decoder.pos = pos;
  return s;
};
const peekVarInt = (decoder) => {
  const pos = decoder.pos;
  const s = readVarInt(decoder);
  decoder.pos = pos;
  return s;
};
const _readVarStringPolyfill = (decoder) => {
  let remainingLen = readVarUint(decoder);
  if (remainingLen === 0) {
    return "";
  } else {
    let encodedString = String.fromCodePoint(readUint8(decoder));
    if (--remainingLen < 100) {
      while (remainingLen--) {
        encodedString += String.fromCodePoint(readUint8(decoder));
      }
    } else {
      while (remainingLen > 0) {
        const nextLen = remainingLen < 1e4 ? remainingLen : 1e4;
        const bytes = decoder.arr.subarray(decoder.pos, decoder.pos + nextLen);
        decoder.pos += nextLen;
        encodedString += String.fromCodePoint.apply(
          null,
          /** @type {any} */
          bytes
        );
        remainingLen -= nextLen;
      }
    }
    return decodeURIComponent(escape(encodedString));
  }
};
const _readVarStringNative = (decoder) => (
  /** @type any */
  utf8TextDecoder.decode(readVarUint8Array(decoder))
);
const readVarString = utf8TextDecoder ? _readVarStringNative : _readVarStringPolyfill;
const readTerminatedUint8Array = (decoder) => {
  const encoder = createEncoder();
  let b;
  while (true) {
    b = readUint8(decoder);
    if (b === 0) {
      return toUint8Array(encoder);
    }
    if (b === 1) {
      b = readUint8(decoder);
    }
    write(encoder, b);
  }
};
const readTerminatedString = (decoder) => decodeUtf8(readTerminatedUint8Array(decoder));
const peekVarString = (decoder) => {
  const pos = decoder.pos;
  const s = readVarString(decoder);
  decoder.pos = pos;
  return s;
};
const readFromDataView = (decoder, len) => {
  const dv = new DataView(decoder.arr.buffer, decoder.arr.byteOffset + decoder.pos, len);
  decoder.pos += len;
  return dv;
};
const readFloat32 = (decoder) => readFromDataView(decoder, 4).getFloat32(0, false);
const readFloat64 = (decoder) => readFromDataView(decoder, 8).getFloat64(0, false);
const readBigInt64 = (decoder) => (
  /** @type {any} */
  readFromDataView(decoder, 8).getBigInt64(0, false)
);
const readBigUint64 = (decoder) => (
  /** @type {any} */
  readFromDataView(decoder, 8).getBigUint64(0, false)
);
const readAnyLookupTable = [
  (decoder) => void 0,
  // CASE 127: undefined
  (decoder) => null,
  // CASE 126: null
  readVarInt,
  // CASE 125: integer
  readFloat32,
  // CASE 124: float32
  readFloat64,
  // CASE 123: float64
  readBigInt64,
  // CASE 122: bigint
  (decoder) => false,
  // CASE 121: boolean (false)
  (decoder) => true,
  // CASE 120: boolean (true)
  readVarString,
  // CASE 119: string
  (decoder) => {
    const len = readVarUint(decoder);
    const obj = {};
    for (let i = 0; i < len; i++) {
      const key = readVarString(decoder);
      obj[key] = readAny(decoder);
    }
    return obj;
  },
  (decoder) => {
    const len = readVarUint(decoder);
    const arr = [];
    for (let i = 0; i < len; i++) {
      arr.push(readAny(decoder));
    }
    return arr;
  },
  readVarUint8Array
  // CASE 116: Uint8Array
];
const readAny = (decoder) => readAnyLookupTable[127 - readUint8(decoder)](decoder);
class RleDecoder extends Decoder {
  /**
   * @param {Uint8Array} uint8Array
   * @param {function(Decoder):T} reader
   */
  constructor(uint8Array, reader) {
    super(uint8Array);
    this.reader = reader;
    this.s = null;
    this.count = 0;
  }
  read() {
    if (this.count === 0) {
      this.s = this.reader(this);
      if (hasContent(this)) {
        this.count = readVarUint(this) + 1;
      } else {
        this.count = -1;
      }
    }
    this.count--;
    return (
      /** @type {T} */
      this.s
    );
  }
}
class IntDiffDecoder extends Decoder {
  /**
   * @param {Uint8Array} uint8Array
   * @param {number} start
   */
  constructor(uint8Array, start) {
    super(uint8Array);
    this.s = start;
  }
  /**
   * @return {number}
   */
  read() {
    this.s += readVarInt(this);
    return this.s;
  }
}
class RleIntDiffDecoder extends Decoder {
  /**
   * @param {Uint8Array} uint8Array
   * @param {number} start
   */
  constructor(uint8Array, start) {
    super(uint8Array);
    this.s = start;
    this.count = 0;
  }
  /**
   * @return {number}
   */
  read() {
    if (this.count === 0) {
      this.s += readVarInt(this);
      if (hasContent(this)) {
        this.count = readVarUint(this) + 1;
      } else {
        this.count = -1;
      }
    }
    this.count--;
    return (
      /** @type {number} */
      this.s
    );
  }
}
class UintOptRleDecoder extends Decoder {
  /**
   * @param {Uint8Array} uint8Array
   */
  constructor(uint8Array) {
    super(uint8Array);
    this.s = 0;
    this.count = 0;
  }
  read() {
    if (this.count === 0) {
      this.s = readVarInt(this);
      const isNegative = isNegativeZero(this.s);
      this.count = 1;
      if (isNegative) {
        this.s = -this.s;
        this.count = readVarUint(this) + 2;
      }
    }
    this.count--;
    return (
      /** @type {number} */
      this.s
    );
  }
}
class IncUintOptRleDecoder extends Decoder {
  /**
   * @param {Uint8Array} uint8Array
   */
  constructor(uint8Array) {
    super(uint8Array);
    this.s = 0;
    this.count = 0;
  }
  read() {
    if (this.count === 0) {
      this.s = readVarInt(this);
      const isNegative = isNegativeZero(this.s);
      this.count = 1;
      if (isNegative) {
        this.s = -this.s;
        this.count = readVarUint(this) + 2;
      }
    }
    this.count--;
    return (
      /** @type {number} */
      this.s++
    );
  }
}
class IntDiffOptRleDecoder extends Decoder {
  /**
   * @param {Uint8Array} uint8Array
   */
  constructor(uint8Array) {
    super(uint8Array);
    this.s = 0;
    this.count = 0;
    this.diff = 0;
  }
  /**
   * @return {number}
   */
  read() {
    if (this.count === 0) {
      const diff = readVarInt(this);
      const hasCount = diff & 1;
      this.diff = floor(diff / 2);
      this.count = 1;
      if (hasCount) {
        this.count = readVarUint(this) + 2;
      }
    }
    this.s += this.diff;
    this.count--;
    return this.s;
  }
}
class StringDecoder {
  /**
   * @param {Uint8Array} uint8Array
   */
  constructor(uint8Array) {
    this.decoder = new UintOptRleDecoder(uint8Array);
    this.str = readVarString(this.decoder);
    this.spos = 0;
  }
  /**
   * @return {string}
   */
  read() {
    const end = this.spos + this.decoder.read();
    const res = this.str.slice(this.spos, end);
    this.spos = end;
    return res;
  }
}
const decoding = /* @__PURE__ */ Object.freeze(/* @__PURE__ */ Object.defineProperty({
  __proto__: null,
  Decoder,
  IncUintOptRleDecoder,
  IntDiffDecoder,
  IntDiffOptRleDecoder,
  RleDecoder,
  RleIntDiffDecoder,
  StringDecoder,
  UintOptRleDecoder,
  _readVarStringNative,
  _readVarStringPolyfill,
  clone,
  createDecoder,
  hasContent,
  peekUint16,
  peekUint32,
  peekUint8,
  peekVarInt,
  peekVarString,
  peekVarUint,
  readAny,
  readBigInt64,
  readBigUint64,
  readFloat32,
  readFloat64,
  readFromDataView,
  readTailAsUint8Array,
  readTerminatedString,
  readTerminatedUint8Array,
  readUint16,
  readUint32,
  readUint32BigEndian,
  readUint8,
  readUint8Array,
  readVarInt,
  readVarString,
  readVarUint,
  readVarUint8Array,
  skip8
}, Symbol.toStringTag, { value: "Module" }));
const messageYjsSyncStep1 = 0;
const messageYjsSyncStep2 = 1;
const messageYjsUpdate = 2;
const writeSyncStep1 = (encoder, doc) => {
  writeVarUint(encoder, messageYjsSyncStep1);
  const sv = Y.encodeStateVector(doc);
  writeVarUint8Array(encoder, sv);
};
const writeSyncStep2 = (encoder, doc, encodedStateVector) => {
  writeVarUint(encoder, messageYjsSyncStep2);
  writeVarUint8Array(encoder, Y.encodeStateAsUpdate(doc, encodedStateVector));
};
const readSyncStep1 = (decoder, encoder, doc) => writeSyncStep2(encoder, doc, readVarUint8Array(decoder));
const readSyncStep2 = (decoder, doc, transactionOrigin, errorHandler) => {
  try {
    Y.applyUpdate(doc, readVarUint8Array(decoder), transactionOrigin);
  } catch (error) {
    if (errorHandler != null) errorHandler(
      /** @type {Error} */
      error
    );
    console.error("Caught error while handling a Yjs update", error);
  }
};
const writeUpdate = (encoder, update) => {
  writeVarUint(encoder, messageYjsUpdate);
  writeVarUint8Array(encoder, update);
};
const readUpdate = readSyncStep2;
const readSyncMessage = (decoder, encoder, doc, transactionOrigin, errorHandler) => {
  const messageType = readVarUint(decoder);
  switch (messageType) {
    case messageYjsSyncStep1:
      readSyncStep1(decoder, encoder, doc);
      break;
    case messageYjsSyncStep2:
      readSyncStep2(decoder, doc, transactionOrigin, errorHandler);
      break;
    case messageYjsUpdate:
      readUpdate(decoder, doc, transactionOrigin, errorHandler);
      break;
    default:
      throw new Error("Unknown message type");
  }
  return messageType;
};
const sync = /* @__PURE__ */ Object.freeze(/* @__PURE__ */ Object.defineProperty({
  __proto__: null,
  messageYjsSyncStep1,
  messageYjsSyncStep2,
  messageYjsUpdate,
  readSyncMessage,
  readSyncStep1,
  readSyncStep2,
  readUpdate,
  writeSyncStep1,
  writeSyncStep2,
  writeUpdate
}, Symbol.toStringTag, { value: "Module" }));
const getUnixTime = Date.now;
const create = () => /* @__PURE__ */ new Map();
const setIfUndefined = (map, key, createT) => {
  let set2 = map.get(key);
  if (set2 === void 0) {
    map.set(key, set2 = createT());
  }
  return set2;
};
class Observable {
  constructor() {
    this._observers = create();
  }
  /**
   * @param {N} name
   * @param {function} f
   */
  on(name, f) {
    setIfUndefined(this._observers, name, create$2).add(f);
  }
  /**
   * @param {N} name
   * @param {function} f
   */
  once(name, f) {
    const _f = (...args) => {
      this.off(name, _f);
      f(...args);
    };
    this.on(name, _f);
  }
  /**
   * @param {N} name
   * @param {function} f
   */
  off(name, f) {
    const observers = this._observers.get(name);
    if (observers !== void 0) {
      observers.delete(f);
      if (observers.size === 0) {
        this._observers.delete(name);
      }
    }
  }
  /**
   * Emit a named event. All registered event listeners that listen to the
   * specified name will receive the event.
   *
   * @todo This should catch exceptions
   *
   * @param {N} name The event name.
   * @param {Array<any>} args The arguments that are applied to the event listener.
   */
  emit(name, args) {
    return from((this._observers.get(name) || create()).values()).forEach((f) => f(...args));
  }
  destroy() {
    this._observers = create();
  }
}
const EqualityTraitSymbol = Symbol("Equality");
const keys = Object.keys;
const size = (obj) => keys(obj).length;
const hasProperty = (obj, key) => Object.prototype.hasOwnProperty.call(obj, key);
const equalityDeep = (a, b) => {
  if (a === b) {
    return true;
  }
  if (a == null || b == null || a.constructor !== b.constructor && (a.constructor || Object) !== (b.constructor || Object)) {
    return false;
  }
  if (a[EqualityTraitSymbol] != null) {
    return a[EqualityTraitSymbol](b);
  }
  switch (a.constructor) {
    case ArrayBuffer:
      a = new Uint8Array(a);
      b = new Uint8Array(b);
    // eslint-disable-next-line no-fallthrough
    case Uint8Array: {
      if (a.byteLength !== b.byteLength) {
        return false;
      }
      for (let i = 0; i < a.length; i++) {
        if (a[i] !== b[i]) {
          return false;
        }
      }
      break;
    }
    case Set: {
      if (a.size !== b.size) {
        return false;
      }
      for (const value of a) {
        if (!b.has(value)) {
          return false;
        }
      }
      break;
    }
    case Map: {
      if (a.size !== b.size) {
        return false;
      }
      for (const key of a.keys()) {
        if (!b.has(key) || !equalityDeep(a.get(key), b.get(key))) {
          return false;
        }
      }
      break;
    }
    case void 0:
    case Object:
      if (size(a) !== size(b)) {
        return false;
      }
      for (const key in a) {
        if (!hasProperty(a, key) || !equalityDeep(a[key], b[key])) {
          return false;
        }
      }
      break;
    case Array:
      if (a.length !== b.length) {
        return false;
      }
      for (let i = 0; i < a.length; i++) {
        if (!equalityDeep(a[i], b[i])) {
          return false;
        }
      }
      break;
    default:
      return false;
  }
  return true;
};
const outdatedTimeout = 3e4;
class Awareness extends Observable {
  /**
   * @param {Y.Doc} doc
   */
  constructor(doc) {
    super();
    this.doc = doc;
    this.clientID = doc.clientID;
    this.states = /* @__PURE__ */ new Map();
    this.meta = /* @__PURE__ */ new Map();
    this._checkInterval = /** @type {any} */
    setInterval(() => {
      const now = getUnixTime();
      if (this.getLocalState() !== null && outdatedTimeout / 2 <= now - /** @type {{lastUpdated:number}} */
      this.meta.get(this.clientID).lastUpdated) {
        this.setLocalState(this.getLocalState());
      }
      const remove = [];
      this.meta.forEach((meta, clientid) => {
        if (clientid !== this.clientID && outdatedTimeout <= now - meta.lastUpdated && this.states.has(clientid)) {
          remove.push(clientid);
        }
      });
      if (remove.length > 0) {
        removeAwarenessStates(this, remove, "timeout");
      }
    }, floor(outdatedTimeout / 10));
    doc.on("destroy", () => {
      this.destroy();
    });
    this.setLocalState({});
  }
  destroy() {
    this.emit("destroy", [this]);
    this.setLocalState(null);
    super.destroy();
    clearInterval(this._checkInterval);
  }
  /**
   * @return {Object<string,any>|null}
   */
  getLocalState() {
    return this.states.get(this.clientID) || null;
  }
  /**
   * @param {Object<string,any>|null} state
   */
  setLocalState(state) {
    const clientID = this.clientID;
    const currLocalMeta = this.meta.get(clientID);
    const clock = currLocalMeta === void 0 ? 0 : currLocalMeta.clock + 1;
    const prevState = this.states.get(clientID);
    if (state === null) {
      this.states.delete(clientID);
    } else {
      this.states.set(clientID, state);
    }
    this.meta.set(clientID, {
      clock,
      lastUpdated: getUnixTime()
    });
    const added = [];
    const updated = [];
    const filteredUpdated = [];
    const removed = [];
    if (state === null) {
      removed.push(clientID);
    } else if (prevState == null) {
      if (state != null) {
        added.push(clientID);
      }
    } else {
      updated.push(clientID);
      if (!equalityDeep(prevState, state)) {
        filteredUpdated.push(clientID);
      }
    }
    if (added.length > 0 || filteredUpdated.length > 0 || removed.length > 0) {
      this.emit("change", [{ added, updated: filteredUpdated, removed }, "local"]);
    }
    this.emit("update", [{ added, updated, removed }, "local"]);
  }
  /**
   * @param {string} field
   * @param {any} value
   */
  setLocalStateField(field, value) {
    const state = this.getLocalState();
    if (state !== null) {
      this.setLocalState({
        ...state,
        [field]: value
      });
    }
  }
  /**
   * @return {Map<number,Object<string,any>>}
   */
  getStates() {
    return this.states;
  }
}
const removeAwarenessStates = (awareness2, clients, origin) => {
  const removed = [];
  for (let i = 0; i < clients.length; i++) {
    const clientID = clients[i];
    if (awareness2.states.has(clientID)) {
      awareness2.states.delete(clientID);
      if (clientID === awareness2.clientID) {
        const curMeta = (
          /** @type {MetaClientState} */
          awareness2.meta.get(clientID)
        );
        awareness2.meta.set(clientID, {
          clock: curMeta.clock + 1,
          lastUpdated: getUnixTime()
        });
      }
      removed.push(clientID);
    }
  }
  if (removed.length > 0) {
    awareness2.emit("change", [{ added: [], updated: [], removed }, origin]);
    awareness2.emit("update", [{ added: [], updated: [], removed }, origin]);
  }
};
const encodeAwarenessUpdate = (awareness2, clients, states = awareness2.states) => {
  const len = clients.length;
  const encoder = createEncoder();
  writeVarUint(encoder, len);
  for (let i = 0; i < len; i++) {
    const clientID = clients[i];
    const state = states.get(clientID) || null;
    const clock = (
      /** @type {MetaClientState} */
      awareness2.meta.get(clientID).clock
    );
    writeVarUint(encoder, clientID);
    writeVarUint(encoder, clock);
    writeVarString(encoder, JSON.stringify(state));
  }
  return toUint8Array(encoder);
};
const modifyAwarenessUpdate = (update, modify) => {
  const decoder = createDecoder(update);
  const encoder = createEncoder();
  const len = readVarUint(decoder);
  writeVarUint(encoder, len);
  for (let i = 0; i < len; i++) {
    const clientID = readVarUint(decoder);
    const clock = readVarUint(decoder);
    const state = JSON.parse(readVarString(decoder));
    const modifiedState = modify(state);
    writeVarUint(encoder, clientID);
    writeVarUint(encoder, clock);
    writeVarString(encoder, JSON.stringify(modifiedState));
  }
  return toUint8Array(encoder);
};
const applyAwarenessUpdate = (awareness2, update, origin) => {
  const decoder = createDecoder(update);
  const timestamp = getUnixTime();
  const added = [];
  const updated = [];
  const filteredUpdated = [];
  const removed = [];
  const len = readVarUint(decoder);
  for (let i = 0; i < len; i++) {
    const clientID = readVarUint(decoder);
    let clock = readVarUint(decoder);
    const state = JSON.parse(readVarString(decoder));
    const clientMeta = awareness2.meta.get(clientID);
    const prevState = awareness2.states.get(clientID);
    const currClock = clientMeta === void 0 ? 0 : clientMeta.clock;
    if (currClock < clock || currClock === clock && state === null && awareness2.states.has(clientID)) {
      if (state === null) {
        if (clientID === awareness2.clientID && awareness2.getLocalState() != null) {
          clock++;
        } else {
          awareness2.states.delete(clientID);
        }
      } else {
        awareness2.states.set(clientID, state);
      }
      awareness2.meta.set(clientID, {
        clock,
        lastUpdated: timestamp
      });
      if (clientMeta === void 0 && state !== null) {
        added.push(clientID);
      } else if (clientMeta !== void 0 && state === null) {
        removed.push(clientID);
      } else if (state !== null) {
        if (!equalityDeep(state, prevState)) {
          filteredUpdated.push(clientID);
        }
        updated.push(clientID);
      }
    }
  }
  if (added.length > 0 || filteredUpdated.length > 0 || removed.length > 0) {
    awareness2.emit("change", [{
      added,
      updated: filteredUpdated,
      removed
    }, origin]);
  }
  if (added.length > 0 || updated.length > 0 || removed.length > 0) {
    awareness2.emit("update", [{
      added,
      updated,
      removed
    }, origin]);
  }
};
const awareness = /* @__PURE__ */ Object.freeze(/* @__PURE__ */ Object.defineProperty({
  __proto__: null,
  Awareness,
  applyAwarenessUpdate,
  encodeAwarenessUpdate,
  modifyAwarenessUpdate,
  outdatedTimeout,
  removeAwarenessStates
}, Symbol.toStringTag, { value: "Module" }));
const Doc = Y.Doc;
const Text = Y.Text;
export {
  Doc,
  Text,
  Y,
  awareness as awarenessProtocol,
  decoding,
  encoding,
  sync as syncProtocol
};
//# sourceMappingURL=crdt.es.js.map
