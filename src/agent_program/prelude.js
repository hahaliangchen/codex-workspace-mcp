// Internal resumable implementations. The receiver and callback are ordinary
// VM values so calls and captured environments survive host suspension.
function $$map(a, f) {
  const n = a.length; const out = []; out.length = n;
  for (let i = 0; i < n; i++) if ($$has(a, i)) out[i] = f(a[i], i, a);
  return out;
}
function $$filter(a, f) {
  const n = a.length; const out = [];
  for (let i = 0; i < n; i++) if ($$has(a, i)) { const value = a[i]; if (f(value, i, a)) out.push(value); }
  return out;
}
function $$forEach(a, f) { const n = a.length; for (let i = 0; i < n; i++) if ($$has(a, i)) f(a[i], i, a); }
function $$some(a, f) { const n = a.length; for (let i = 0; i < n; i++) if ($$has(a, i) && f(a[i], i, a)) return true; return false; }
function $$every(a, f) { const n = a.length; for (let i = 0; i < n; i++) if ($$has(a, i) && !f(a[i], i, a)) return false; return true; }
function $$find(a, f) { const n = a.length; for (let i = 0; i < n; i++) { const value = a[i]; if (f(value, i, a)) return value; } }
function $$findIndex(a, f) { const n = a.length; for (let i = 0; i < n; i++) if (f(a[i], i, a)) return i; return -1; }
function $$findLast(a, f) { for (let i = a.length - 1; i >= 0; i--) { const value = a[i]; if (f(value, i, a)) return value; } }
function $$findLastIndex(a, f) { for (let i = a.length - 1; i >= 0; i--) if (f(a[i], i, a)) return i; return -1; }
function $$flatMap(a, f) { return $$map(a, f).flat(1); }
function $$reduce(a, f, initial, supplied) {
  const n = a.length; let i = 0; let result = initial;
  if (!supplied) { while (i < n && !$$has(a, i)) i++; if (i >= n) throw new TypeError('Reduce of empty array with no initial value'); result = a[i]; i++; }
  for (; i < n; i++) if ($$has(a, i)) result = f(result, a[i], i, a);
  return result;
}
function $$reduceRight(a, f, initial, supplied) {
  let i = a.length - 1; let result = initial;
  if (!supplied) { while (i >= 0 && !$$has(a, i)) i--; if (i < 0) throw new TypeError('Reduce of empty array with no initial value'); result = a[i]; i--; }
  for (; i >= 0; i--) if ($$has(a, i)) result = f(result, a[i], i, a);
  return result;
}
function $$sortCore(a, compare, copy) {
  const n = a.length; let values = []; let missing = 0;
  for (let i = 0; i < n; i++) {
    if (copy || $$has(a, i)) {
      const value = a[i];
      if (value === undefined) missing++; else values.push(value);
    }
  }
  const count = values.length; let temp = [];
  for (let width = 1; width < count; width *= 2) {
    for (let i = 0; i < count; i += width * 2) {
      let left = i; let right = Math.min(i + width, count);
      const middle = right; const end = Math.min(i + width * 2, count);
      for (let k = i; k < end; k++) {
        if (left >= middle) { temp[k] = values[right]; right++; }
        else if (right >= end) { temp[k] = values[left]; left++; }
        else if (+compare(values[left], values[right]) > 0) { temp[k] = values[right]; right++; }
        else { temp[k] = values[left]; left++; }
      }
    }
    const swap = values; values = temp; temp = swap;
  }
  for (let i = 0; i < missing; i++) values.push(undefined);
  if (copy) return values;
  return $$sortWriteBack(a, values);
}
function $$sort(a, compare) { return $$sortCore(a, compare, false); }
function $$toSorted(a, compare) { return $$sortCore(a, compare, true); }
function $$replace(text, pattern, replacement, all) {
  const matches = $$replacementMatches(text, pattern, all);
  let out = ''; let previous = 0;
  for (let i = 0; i < matches.length; i++) {
    const match = matches[i];
    out += text.slice(previous, match.start);
    match.args.push(match.start, text);
    if (match.hasGroups) match.args.push(match.groups);
    out = $$textLimit(out + String($$apply(replacement, match.args)));
    previous = match.end;
  }
  return $$textLimit(out + text.slice(previous));
}

// Promise combinators are ordinary resumable JavaScript functions.
function $$promiseAll(values) {
  return new Promise((resolve, reject) => {
    let remaining = values.length; const result = []; result.length = remaining;
    if (remaining === 0) { resolve(result); return; }
    for (let i = 0; i < values.length; i++) {
      Promise.resolve(values[i]).then(value => {
        result[i] = value; remaining--; if (remaining === 0) resolve(result);
      }, reject);
    }
  });
}
function $$promiseAllSettled(values) {
  return new Promise(resolve => {
    let remaining = values.length; const result = []; result.length = remaining;
    if (remaining === 0) { resolve(result); return; }
    for (let i = 0; i < values.length; i++) {
      Promise.resolve(values[i]).then(value => {
        result[i] = {status:'fulfilled', value}; remaining--; if (remaining === 0) resolve(result);
      }, reason => {
        result[i] = {status:'rejected', reason}; remaining--; if (remaining === 0) resolve(result);
      });
    }
  });
}
function $$promiseRace(values) {
  return new Promise((resolve, reject) => {
    for (let i = 0; i < values.length; i++) Promise.resolve(values[i]).then(resolve, reject);
  });
}
function $$promiseAny(values) {
  return new Promise((resolve, reject) => {
    let remaining = values.length; const errors = []; errors.length = remaining;
    if (remaining === 0) { const e = new Error('All promises were rejected'); e.name = 'AggregateError'; e.errors = errors; reject(e); return; }
    for (let i = 0; i < values.length; i++) {
      Promise.resolve(values[i]).then(resolve, reason => {
        errors[i] = reason; remaining--;
        if (remaining === 0) { const e = new Error('All promises were rejected'); e.name = 'AggregateError'; e.errors = errors; reject(e); }
      });
    }
  });
}
function $$promiseFinally(promise, callback) {
  if (typeof callback !== 'function') return promise.then(callback, callback);
  return promise.then(value => Promise.resolve(callback()).then(() => value),
    reason => Promise.resolve(callback()).then(() => { throw reason; }));
}
