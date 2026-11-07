<!-- Modified by alfypops, 2026: the fork ships no GitHub workflows; the threshold is checked locally -->
# Code Coverage Policy

## Overview

This project maintains a minimum code coverage threshold to ensure code quality and prevent regressions in test coverage.

## Current Threshold

**Minimum Coverage: 50%** (line coverage)

This fork ships no GitHub workflows, so nothing enforces the threshold in CI. Check it locally with `task coverage:check`, which defaults to 50% (`COVERAGE_THRESHOLD` in `Taskfile.yml`).

## How the Threshold is Determined

The threshold is calculated as:
1. Measure current total code coverage percentage
2. Round down to the nearest multiple of 5
3. Set this as the minimum threshold

**Example:** If current coverage is 54.10%, the threshold is set to 50%.

## Checking Coverage Locally

### Quick Summary
```bash
task coverage
```

### HTML Report (Interactive)
```bash
task coverage:html
```

### LCOV Report (for tools/IDEs)
```bash
task coverage:lcov
```

### Check Against Threshold
```bash
task coverage:check
```

## Updating the Threshold

When code coverage improves significantly, raise the default `COVERAGE_THRESHOLD` in the `coverage:check` task in `Taskfile.yml` to the current coverage rounded down to the nearest 5%. To try a value without editing, run `task coverage:check COVERAGE_THRESHOLD=55`.

## Exclusions

The following are excluded from coverage calculation:
- Test files (`tests/**`)
- Benchmarks (`benches/**`)
- Examples (`examples/**`)

This ensures coverage metrics focus on production code quality.

## Coverage Reports

`task coverage:html` and `task coverage:lcov` write the reports locally.

## Best Practices

1. **Write tests for new code**: Aim to maintain or improve coverage with each PR
2. **Review coverage reports**: Check which lines aren't covered and consider adding tests
3. **Don't game the metrics**: Focus on meaningful test coverage, not just hitting numbers
4. **Update threshold periodically**: As coverage improves, update the threshold to lock in gains
