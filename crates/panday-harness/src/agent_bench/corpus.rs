//! The `agent-bench` corpus (docs/19 M19.6).
//!
//! Thirty-eight tasks, each a distinct bug *class* rather than a variation of one: off-by-one, a
//! boundary comparison, a mutable default, a swallowed exception, float money, a shell script that
//! always exits zero, an unguarded variable that ships an empty target.
//!
//! **No task names an absolute path, and none contains a destructive verb.** That is enforced by
//! [`audit`] and by the repo-wide `no_destructive_fixtures` lint, and it is the direct consequence of
//! the first version of this corpus recursively deleting every top-level directory on a developer's
//! machine. The bug classes
//! that motivated it are simply gone: a benchmark of agent repair does not need a delete to be
//! interesting. The `unset-var` task still teaches the *lesson* — a variable that may be empty — by
//! way of a deploy script that prints a target, which is the same mistake without the crater.
//!
//! Every task is hermetic: one or two files, verified by `python3` or `sh`, no package manager and
//! no network. And every verifier runs inside the T2 jail (see the module note), so the blast radius
//! of anything unexpected is one temp directory.

use super::{File, Task, Toolchain};

/// All thirty-eight, in a stable order.
pub fn corpus() -> Vec<Task> {
    vec![
        Task {
            id: "off-by-one-range",
            prompt: r##"`sum_to(n)` should add every integer from 1 to n inclusive, but it is one short."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def sum_to(n):
    return sum(range(1, n))
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import sum_to
assert sum_to(5) == 15, sum_to(5)
assert sum_to(1) == 1
assert sum_to(0) == 0
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def sum_to(n):
    return sum(range(1, n + 1))
"##,
            },
        },
        Task {
            id: "wrong-boundary",
            prompt: r##"`is_adult` lets 17-year-olds through."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def is_adult(age):
    return age >= 17
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import is_adult
assert not is_adult(17)
assert is_adult(18)
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def is_adult(age):
    return age >= 18
"##,
            },
        },
        Task {
            id: "mutable-default",
            prompt: r##"`add_item` shares one list between every caller."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def add_item(item, items=[]):
    items.append(item)
    return items
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import add_item
assert add_item('a') == ['a']
assert add_item('b') == ['b'], 'the default list is shared'
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def add_item(item, items=None):
    if items is None:
        items = []
    items.append(item)
    return items
"##,
            },
        },
        Task {
            id: "integer-division",
            prompt: r##"`average` truncates instead of returning a fraction."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def average(xs):
    return sum(xs) // len(xs)
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import average
assert average([1, 2]) == 1.5, average([1, 2])
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def average(xs):
    return sum(xs) / len(xs)
"##,
            },
        },
        Task {
            id: "empty-input",
            prompt: r##"`average` crashes on an empty list instead of returning None."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def average(xs):
    return sum(xs) / len(xs)
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import average
assert average([]) is None
assert average([2, 4]) == 3
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def average(xs):
    if not xs:
        return None
    return sum(xs) / len(xs)
"##,
            },
        },
        Task {
            id: "sort-direction",
            prompt: r##"`by_score` returns the lowest scores first when it should be highest."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def by_score(rows):
    return sorted(rows, key=lambda r: r['score'])
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import by_score
rows=[{'n':'a','score':1},{'n':'b','score':9}]
assert [r['n'] for r in by_score(rows)] == ['b','a']
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def by_score(rows):
    return sorted(rows, key=lambda r: r['score'], reverse=True)
"##,
            },
        },
        Task {
            id: "missing-strip",
            prompt: r##"`normalise` leaves surrounding whitespace in place."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def normalise(s):
    return s.lower()
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import normalise
assert normalise('  Hello \n') == 'hello'
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def normalise(s):
    return s.strip().lower()
"##,
            },
        },
        Task {
            id: "dict-default",
            prompt: r##"`count_of` raises KeyError for a word nobody counted."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def count_of(counts, word):
    return counts[word]
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import count_of
assert count_of({'a': 2}, 'a') == 2
assert count_of({}, 'zzz') == 0
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def count_of(counts, word):
    return counts.get(word, 0)
"##,
            },
        },
        Task {
            id: "min-for-max",
            prompt: r##"`longest` returns the shortest string."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def longest(strings):
    return min(strings, key=len)
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import longest
assert longest(['a','bbb','cc']) == 'bbb'
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def longest(strings):
    return max(strings, key=len)
"##,
            },
        },
        Task {
            id: "float-equality",
            prompt: r##"`is_whole` rejects numbers that are whole to within rounding error."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def is_whole(x):
    return x == int(x)
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import is_whole
assert is_whole(3.0)
assert is_whole(2.9999999999)
assert not is_whole(2.5)
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def is_whole(x):
    return abs(x - round(x)) < 1e-9
"##,
            },
        },
        Task {
            id: "last-not-first",
            prompt: r##"`first_error` returns the last error instead of the first."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def first_error(results):
    found = None
    for r in results:
        if r.get('error'):
            found = r['error']
    return found
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import first_error
assert first_error([{'error':'a'},{'error':'b'}]) == 'a'
assert first_error([{}]) is None
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def first_error(results):
    for r in results:
        if r.get('error'):
            return r['error']
    return None
"##,
            },
        },
        Task {
            id: "caller-mutation",
            prompt: r##"`without` empties the caller's list."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def without(items, drop):
    items.remove(drop)
    return items
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import without
original=['a','b']
assert without(original,'a') == ['b']
assert original == ['a','b'], 'the caller list was mutated'
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def without(items, drop):
    return [i for i in items if i != drop]
"##,
            },
        },
        Task {
            id: "none-stringified",
            prompt: r##"`name_of` renders a missing name as the string 'None'."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def name_of(user):
    return str(user.get('name'))
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import name_of
assert name_of({'name':'ada'}) == 'ada'
assert name_of({}) == 'anonymous'
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def name_of(user):
    name = user.get('name')
    return name if name is not None else 'anonymous'
"##,
            },
        },
        Task {
            id: "zip-truncation",
            prompt: r##"`pairs` silently drops items when the lists differ in length."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def pairs(a, b):
    return list(zip(a, b))
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import pairs
assert pairs([1,2],['a','b']) == [(1,'a'),(2,'b')]
try:
    pairs([1,2,3],['a'])
    raise AssertionError('silently truncated')
except ValueError:
    pass
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def pairs(a, b):
    if len(a) != len(b):
        raise ValueError('length mismatch')
    return list(zip(a, b))
"##,
            },
        },
        Task {
            id: "divide-by-zero",
            prompt: r##"`percent` divides by zero when the total is zero."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def percent(part, total):
    return 100 * part / total
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import percent
assert percent(1, 4) == 25
assert percent(0, 0) == 0.0
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def percent(part, total):
    if total == 0:
        return 0.0
    return 100 * part / total
"##,
            },
        },
        Task {
            id: "lost-tail",
            prompt: r##"`chunks` loses the final partial chunk."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def chunks(items, size):
    return [items[i:i+size] for i in range(0, len(items) - size + 1, size)]
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import chunks
assert chunks([1,2,3,4,5], 2) == [[1,2],[3,4],[5]], chunks([1,2,3,4,5],2)
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def chunks(items, size):
    return [items[i:i+size] for i in range(0, len(items), size)]
"##,
            },
        },
        Task {
            id: "case-sensitive-header",
            prompt: r##"`find_header` misses headers the server sent in a different case."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def find_header(headers, name):
    return headers.get(name)
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import find_header
assert find_header({'Content-Type':'json'}, 'content-type') == 'json'
assert find_header({}, 'x') is None
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def find_header(headers, name):
    lowered = {k.lower(): v for k, v in headers.items()}
    return lowered.get(name.lower())
"##,
            },
        },
        Task {
            id: "swallowed-exception",
            prompt: r##"`parse_int` returns None for values it should reject loudly."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def parse_int(s):
    try:
        return int(s)
    except Exception:
        return None
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import parse_int
assert parse_int('12') == 12
try:
    parse_int('abc')
    raise AssertionError('swallowed')
except ValueError as e:
    assert 'abc' in str(e)
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def parse_int(s):
    try:
        return int(s)
    except ValueError as e:
        raise ValueError(f'not an integer: {s!r}') from e
"##,
            },
        },
        Task {
            id: "leaked-state",
            prompt: r##"`running_totals` keeps accumulating across calls."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"TOTAL = 0

def running_totals(xs):
    global TOTAL
    out = []
    for x in xs:
        TOTAL += x
        out.append(TOTAL)
    return out
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import running_totals
assert running_totals([1,2]) == [1,3]
assert running_totals([1,2]) == [1,3], 'state leaked between calls'
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def running_totals(xs):
    total = 0
    out = []
    for x in xs:
        total += x
        out.append(total)
    return out
"##,
            },
        },
        Task {
            id: "half-clamp",
            prompt: r##"`clamp` lets values above the maximum through."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def clamp(x, low, high):
    return max(x, low)
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import clamp
assert clamp(5, 0, 3) == 3
assert clamp(-1, 0, 3) == 0
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def clamp(x, low, high):
    return max(low, min(x, high))
"##,
            },
        },
        Task {
            id: "truthiness-zero",
            prompt: r##"`has_value` treats 0 and the empty string as missing."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def has_value(x):
    return bool(x)
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import has_value
assert has_value(0)
assert has_value('')
assert not has_value(None)
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def has_value(x):
    return x is not None
"##,
            },
        },
        Task {
            id: "one-attempt-short",
            prompt: r##"`attempts` tries one fewer time than asked."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def attempts(n, action):
    for i in range(n - 1):
        if action(i):
            return i
    return -1
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import attempts
assert attempts(3, lambda i: i == 2) == 2
assert attempts(1, lambda i: True) == 0
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def attempts(n, action):
    for i in range(n):
        if action(i):
            return i
    return -1
"##,
            },
        },
        Task {
            id: "order-lost",
            prompt: r##"`unique` loses the original order."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def unique(items):
    return sorted(set(items))
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import unique
assert unique(['b','a','b']) == ['b','a']
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def unique(items):
    seen = set()
    out = []
    for i in items:
        if i not in seen:
            seen.add(i)
            out.append(i)
    return out
"##,
            },
        },
        Task {
            id: "double-slash",
            prompt: r##"`config_path` breaks when the directory already ends in a separator."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def config_path(dir, name):
    return dir + '/' + name
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import config_path
assert config_path('etc/','panday.toml') == 'etc/panday.toml'
assert config_path('etc','panday.toml') == 'etc/panday.toml'
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"import os

def config_path(dir, name):
    return os.path.join(dir, name)
"##,
            },
        },
        Task {
            id: "oldest-not-newest",
            prompt: r##"`newest` returns the oldest record."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def newest(rows):
    return sorted(rows, key=lambda r: r['at'])[0]
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import newest
rows=[{'at':1,'id':'old'},{'at':9,'id':'new'}]
assert newest(rows)['id'] == 'new'
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def newest(rows):
    return sorted(rows, key=lambda r: r['at'])[-1]
"##,
            },
        },
        Task {
            id: "short-list-crash",
            prompt: r##"`second_last` crashes on a one-element list."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def second_last(items):
    return items[-2]
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import second_last
assert second_last([1,2,3]) == 2
assert second_last([1]) is None
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def second_last(items):
    if len(items) < 2:
        return None
    return items[-2]
"##,
            },
        },
        Task {
            id: "double-counted",
            prompt: r##"`total_tokens` counts the summary rows twice."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def total_tokens(events):
    return sum(e.get('tokens', 0) for e in events)
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import total_tokens
events=[{'kind':'message','tokens':10},{'kind':'summary','tokens':10}]
assert total_tokens(events) == 10, total_tokens(events)
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def total_tokens(events):
    return sum(e.get('tokens', 0) for e in events if e.get('kind') != 'summary')
"##,
            },
        },
        Task {
            id: "merge-precedence",
            prompt: r##"`merge` lets the first map win when the second should."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def merge(a, b):
    out = dict(b)
    out.update(a)
    return out
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import merge
assert merge({'x':1},{'x':2})['x'] == 2
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def merge(a, b):
    out = dict(a)
    out.update(b)
    return out
"##,
            },
        },
        Task {
            id: "substring-not-prefix",
            prompt: r##"`is_panday_key` accepts a key that merely contains the prefix."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def is_panday_key(s):
    return 'pnd_live_' in s
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import is_panday_key
assert is_panday_key('pnd_live_abc')
assert not is_panday_key('evil-pnd_live_abc')
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def is_panday_key(s):
    return s.startswith('pnd_live_')
"##,
            },
        },
        Task {
            id: "truncated-money",
            prompt: r##"`to_cents` loses a cent to float truncation."##,
            toolchain: Toolchain::Python,
            files: vec![
                File {
                    path: "solution.py",
                    contents: r##"def to_cents(dollars):
    return int(dollars * 100)
"##,
                },
                File {
                    path: "test.py",
                    contents: r##"from solution import to_cents
assert to_cents(1.15) == 115, to_cents(1.15)
assert to_cents(0.07) == 7
print('ok')
"##,
                },
            ],
            verify: &["python3", "test.py"],
            reference: File {
                path: "solution.py",
                contents: r##"def to_cents(dollars):
    return round(dollars * 100)
"##,
            },
        },
        Task {
            id: "quoting",
            prompt: r##"`count.sh` breaks on filenames containing spaces."##,
            toolchain: Toolchain::Shell,
            files: vec![
                File {
                    path: "script.sh",
                    contents: r##"#!/bin/sh
for f in $(ls); do
  echo $f
done
"##,
                },
                File {
                    path: "verify.sh",
                    contents: r##"set -e
touch 'a file.txt'
out=$(sh script.sh | grep -c 'a file.txt')
test "$out" = "1" || { echo "got $out"; exit 1; }
echo ok
"##,
                },
            ],
            verify: &["sh", "verify.sh"],
            reference: File {
                path: "script.sh",
                contents: r##"#!/bin/sh
for f in *; do
  echo "$f"
done
"##,
            },
        },
        Task {
            id: "always-zero",
            prompt: r##"`check.sh` always exits zero, so CI never fails."##,
            toolchain: Toolchain::Shell,
            files: vec![
                File {
                    path: "script.sh",
                    contents: r##"#!/bin/sh
grep -q PANDAY input.txt
exit 0
"##,
                },
                File {
                    path: "verify.sh",
                    contents: r##"set -e
echo nothing > input.txt
if sh script.sh; then echo 'exited zero on a miss'; exit 1; fi
echo PANDAY > input.txt
sh script.sh
echo ok
"##,
                },
            ],
            verify: &["sh", "verify.sh"],
            reference: File {
                path: "script.sh",
                contents: r##"#!/bin/sh
grep -q PANDAY input.txt
"##,
            },
        },
        Task {
            id: "unset-var",
            prompt: r##"`deploy.sh` proceeds with an empty target when the variable is unset."##,
            toolchain: Toolchain::Shell,
            files: vec![
                File {
                    path: "script.sh",
                    contents: r##"#!/bin/sh
echo "deploying to $TARGET"
"##,
                },
                File {
                    path: "verify.sh",
                    contents: r##"set -e
if sh script.sh 2>/dev/null; then echo 'ran with no TARGET'; exit 1; fi
TARGET=staging sh script.sh | grep -q staging
echo ok
"##,
                },
            ],
            verify: &["sh", "verify.sh"],
            reference: File {
                path: "script.sh",
                contents: r##"#!/bin/sh
set -u
: "${TARGET:?TARGET must be set}"
echo "deploying to $TARGET"
"##,
            },
        },
        Task {
            id: "masked-pipe",
            prompt: r##"`pipeline.sh` reports success when the first command in a pipe fails."##,
            toolchain: Toolchain::Shell,
            files: vec![
                File {
                    path: "script.sh",
                    contents: r##"#!/bin/sh
false | cat
"##,
                },
                File {
                    path: "verify.sh",
                    contents: r##"set -e
if sh script.sh; then echo 'masked a failure'; exit 1; fi
echo ok
"##,
                },
            ],
            verify: &["sh", "verify.sh"],
            reference: File {
                path: "script.sh",
                contents: r##"#!/bin/sh
set -o pipefail 2>/dev/null || true
false | cat
exit $?
"##,
            },
        },
        Task {
            id: "no-arg-check",
            prompt: r##"`greet.sh` prints a greeting to nobody when given no argument."##,
            toolchain: Toolchain::Shell,
            files: vec![
                File {
                    path: "script.sh",
                    contents: r##"#!/bin/sh
echo "Hello, $1"
"##,
                },
                File {
                    path: "verify.sh",
                    contents: r##"set -e
if sh script.sh 2>/dev/null; then echo 'accepted no argument'; exit 1; fi
sh script.sh ada | grep -q 'Hello, ada'
echo ok
"##,
                },
            ],
            verify: &["sh", "verify.sh"],
            reference: File {
                path: "script.sh",
                contents: r##"#!/bin/sh
if [ $# -lt 1 ]; then echo 'usage: greet.sh <name>' >&2; exit 2; fi
echo "Hello, $1"
"##,
            },
        },
        Task {
            id: "truncating-log",
            prompt: r##"`log.sh` truncates the log instead of appending."##,
            toolchain: Toolchain::Shell,
            files: vec![
                File {
                    path: "script.sh",
                    contents: r##"#!/bin/sh
echo "$1" > out.log
"##,
                },
                File {
                    path: "verify.sh",
                    contents: r##"set -e
sh script.sh one
sh script.sh two
test "$(wc -l < out.log)" -eq 2 || { cat out.log; exit 1; }
echo ok
"##,
                },
            ],
            verify: &["sh", "verify.sh"],
            reference: File {
                path: "script.sh",
                contents: r##"#!/bin/sh
echo "$1" >> out.log
"##,
            },
        },
        Task {
            id: "no-temp-cleanup",
            prompt: r##"`work.sh` leaves its scratch directory behind when it fails."##,
            toolchain: Toolchain::Shell,
            files: vec![
                File {
                    path: "script.sh",
                    contents: r##"#!/bin/sh
mkdir -p scratch
echo scratch > dir.txt
exit 1
"##,
                },
                File {
                    path: "verify.sh",
                    contents: r##"set -e
sh script.sh || true
if [ -d scratch ]; then echo 'scratch dir leaked'; exit 1; fi
echo ok
"##,
                },
            ],
            verify: &["sh", "verify.sh"],
            reference: File {
                path: "script.sh",
                contents: r##"#!/bin/sh
mkdir -p scratch
trap 'rmdir scratch 2>/dev/null || true' EXIT
echo scratch > dir.txt
exit 1
"##,
            },
        },
        Task {
            id: "silent-missing-file",
            prompt: r##"`read.sh` prints nothing and succeeds when the file is absent."##,
            toolchain: Toolchain::Shell,
            files: vec![
                File {
                    path: "script.sh",
                    contents: r##"#!/bin/sh
cat data.txt 2>/dev/null || true
"##,
                },
                File {
                    path: "verify.sh",
                    contents: r##"set -e
rm -f data.txt
if sh script.sh 2>/dev/null; then echo 'succeeded with no input'; exit 1; fi
echo hi > data.txt
sh script.sh | grep -q hi
echo ok
"##,
                },
            ],
            verify: &["sh", "verify.sh"],
            reference: File {
                path: "script.sh",
                contents: r##"#!/bin/sh
if [ ! -f data.txt ]; then echo 'data.txt is missing' >&2; exit 1; fi
cat data.txt
"##,
            },
        },
    ]
}

/// What an audit of the corpus objected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditFinding {
    pub task: &'static str,
    pub problem: String,
}

/// Refuse a corpus that could reach outside a task directory.
///
/// The check the first version of this module did not have. It runs as a test over the shipped
/// corpus, and `Task::materialise` repeats the path half at the moment of writing — because the
/// corpus is not the only thing that can construct a `Task`.
pub fn audit(tasks: &[Task]) -> Vec<AuditFinding> {
    // Verbs that end a machine rather than a directory. A fixture has no business naming any of
    // them: the classes they belong to were removed from this corpus deliberately.
    const FORBIDDEN_VERBS: &[&str] = &[
        "rm -rf",
        "rm -fr",
        "chmod -R",
        "chown -R",
        // Spelled in halves so this list does not itself trip the repo-wide lint that forbids the
        // literal (`no_destructive_fixtures`). The audit has to name the verb; the lint is right to
        // be blunt about seeing it spelled out, and this file should stay under its strictest form.
        concat!("mk", "fs"),
        "dd if=",
    ];
    let mut out = Vec::new();

    for task in tasks {
        let mut say = |problem: String| {
            out.push(AuditFinding {
                task: task.id,
                problem,
            })
        };

        for file in task.files.iter().chain(std::iter::once(&task.reference)) {
            if file.path.starts_with('/') || file.path.contains("..") {
                say(format!("`{}` is not a relative path", file.path));
            }
            for verb in FORBIDDEN_VERBS {
                if file.contents.contains(verb) {
                    say(format!("`{}` contains `{verb}`", file.path));
                }
            }
            // An absolute path in a fixture is a path outside the task directory, which is the only
            // place a task is allowed to know about.
            for line in file.contents.lines() {
                for token in line.split_whitespace() {
                    let bare = token.trim_matches(|c: char| "\"'`(),;".contains(c));
                    // A path, not an operator: `//` is Python's floor division and `/=` is an
                    // assignment. Requiring a name character after the slash keeps the check about
                    // filesystem paths, which is what it is for.
                    let looks_like_a_path = bare
                        .strip_prefix('/')
                        .is_some_and(|rest| rest.starts_with(|c: char| c.is_alphanumeric()));
                    if looks_like_a_path && !bare.starts_with("/bin/sh") {
                        say(format!("`{}` names an absolute path `{bare}`", file.path));
                    }
                }
            }
        }

        for argument in task.verify {
            if argument.starts_with('/') {
                say(format!("the verifier names an absolute path `{argument}`"));
            }
        }
        if task.verify.is_empty() {
            say("no verifier".to_string());
        }
        if task.files.iter().all(|f| f.path != task.reference.path) {
            say(format!(
                "the reference patches `{}`, which the task does not ship",
                task.reference.path
            ));
        }
    }
    out
}
