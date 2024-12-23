'''
Process logs to compute pointwise success rate metrics.

These metrics are measured as follows.  For each function, we run the static
analysis and rewrite that function in isolation, producing a new `.rs` file
where that function has been rewritten but all other code remains the same.
Then we remove the `unsafe` qualifier from the target function and try to
compile the code.  The "pointwise success rate" is the number of functions on
which this procedure succeeds.

As a performance optimization, instead of running analysis separately for each
function, we run `c2rust-analyze` with `--rewrite-mode pointwise`, which runs
the analysis part once and then rewrites each function in isolation using the
same analysis results.  This provides a significant speedup for large codebases
where the static analysis portion is very slow.

To provide a basis for comparison, in addition to attempting to compile all
pointwise rewrites, we also try removing `unsafe` and compiling each function
in the original, unmodified code.  This provides a baseline for how many
functions are "trivially safe" without rewriting.
'''

from pprint import pprint
import re
import sys

# `pointwise_log_path` should be a log generated by running
# `pointwise_try_build.sh` on each output file of a pointwise rewrite
# (`foo.*.rs`, one per function).  The outputs for all files should be
# concatenated in a single log.  This gives the results of pointwise rewriting
# and compiling each function.
#
# `unmodified_log_path` should come from `pointwise_try_build_unmodified.sh`
# instead.  This gives results of pointwise compiling each function without
# rewriting.
pointwise_log_path, unmodified_log_path = sys.argv[1:]


FUNC_ERRORS_RE = re.compile(r'^got ([0-9]+) errors for ([^ \n]+)$')

def read_func_errors(f):
    func_errors = {}
    for line in f:
        m = FUNC_ERRORS_RE.match(line)
        if m is None:
            continue
        func = m.group(2)
        errors = int(m.group(1))
        assert func not in func_errors, 'duplicate entry for %r' % func
        func_errors[func] = errors
    return func_errors

def calc_pct(n, d):
    if d == 0:
        return 0
    else:
        return n / d * 100

pointwise_func_errors = read_func_errors(open(pointwise_log_path))
pointwise_ok = set(func for func, errors in pointwise_func_errors.items() if errors == 0)
print('pointwise:  %5d/%d functions passed (%.1f%%)' % (
    len(pointwise_ok), len(pointwise_func_errors),
    calc_pct(len(pointwise_ok), len(pointwise_func_errors))))

unmodified_func_errors = read_func_errors(open(unmodified_log_path))
unmodified_ok = set(func for func, errors in unmodified_func_errors.items() if errors == 0)
print('unmodified: %5d/%d functions passed (%.1f%%)' % (
    len(unmodified_ok), len(unmodified_func_errors),
    calc_pct(len(unmodified_ok), len(unmodified_func_errors))))

assert len(pointwise_func_errors) == len(unmodified_func_errors)
num_total = len(pointwise_func_errors)
num_unmodified_ok = len(unmodified_ok)
num_unmodified_bad = num_total - num_unmodified_ok

improved = pointwise_ok - unmodified_ok
print('improved:   %5d/%d functions (%.1f%%)' % (
    len(improved), num_unmodified_bad, calc_pct(len(improved), num_unmodified_bad)))
broke = unmodified_ok - pointwise_ok
print('broke:      %5d/%d functions (%.1f%%)' % (
    len(broke), num_unmodified_ok, calc_pct(len(broke), num_unmodified_ok)))
