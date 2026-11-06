# GitLab CI and Jenkins

fermut is a plain CLI, so it runs anywhere a shell does. The
[packaged action](github-action.md) and `fermut pr-comment` are
GitHub-only; everything else on this page is the same CLI the action
wraps. These recipes translate the [generic steps](integrations.md#other-ci-systems)
for GitLab CI and Jenkins.

> **Not CI-tested by fermut.** fermut's own CI runs on GitHub Actions. The
> recipes below use documented GitLab/Jenkins features and fermut flags,
> but adapt image names, runner tags and credentials to your setup.

## Telling a broken run from a failed gate

`fermut run` exits `1` both when the gate fails (a survivor, or a score
under `--fail-under`) and when the run itself errors out (bad config, red
baseline, git failure); see [exit codes](../reference/cli/run.md). A shared
CI template usually wants to tell "tests are weak" from "the job broke".
Split them: run with `--no-fail` so a non-zero exit can only mean the run
broke, then gate on the [JSON report](../reference/json.md):

```sh
fermut run src/ --tests tests/ --no-fail --json report.json  # non-zero = infra error
python - <<'EOF'                                              # non-zero = gate failed
import json, sys
s = json.load(open("report.json"))["summary"]
if s["scored"] == 0:
    sys.exit(0)                      # nothing to score: N/A, not a failure
print(f"mutation score {s['mutation_score']}% over {s['scored']} mutants")
sys.exit(1 if s["mutation_score"] < 80 else 0)
EOF
```

Read `scored` before `mutation_score`: with nothing scored the score is a
placeholder `100.0`. Both scripts below use this split.

## GitLab CI

### Merge request gate

Scores the lines a merge request changed and posts the Markdown report
as an MR note.

```yaml
fermut:
  image: python:3.12
  rules:
    - if: $CI_PIPELINE_SOURCE == "merge_request_event"
  variables:
    GIT_DEPTH: 0                         # full history so --diff-only finds the merge base
  cache:
    key: fermut-$CI_COMMIT_REF_SLUG
    fallback_keys: [fermut-$CI_DEFAULT_BRANCH]
    paths: [.fermut/]                    # result cache + history
  script:
    - pip install fermut pytest coverage pytest-cov -e .
    - git fetch origin "$CI_MERGE_REQUEST_TARGET_BRANCH_NAME"
    - fermut coverage
    - fermut run src/ --tests tests/
        --diff-only "origin/$CI_MERGE_REQUEST_TARGET_BRANCH_NAME"
        --no-verify-baseline --no-fail
        --json report.json --markdown report.md --junit report.xml
    - python ci/fermut_gate.py report.json     # the gate script above
  after_script:
    - >
      [ -n "$FERMUT_NOTE_TOKEN" ] && [ -f report.md ] &&
      curl -sf -X POST
      -H "PRIVATE-TOKEN: $FERMUT_NOTE_TOKEN"
      --data-urlencode "body@report.md"
      "$CI_API_V4_URL/projects/$CI_PROJECT_ID/merge_requests/$CI_MERGE_REQUEST_IID/notes"
      || true
  artifacts:
    when: always
    paths: [report.json, report.md]
    reports:
      junit: report.xml                  # survivors show in the MR test widget
```

- **`--no-verify-baseline`** is safe here because `fermut coverage` ran the
  full suite in the same job. Drop it if you split coverage into its own
  job; see [baseline check in CI](ci-quickstart.md#baseline-check-in-ci).
- **The MR note** needs a project or group access token with the `api`
  scope in a masked variable (`FERMUT_NOTE_TOKEN` here). Without it the
  step is skipped and the report stays an artifact. Each pipeline adds a
  new note; it doesn't edit the previous one.
- **The cache** is keyed per branch with a fallback to the default
  branch, so an MR starts from `main`'s verdicts. To sign the cache, set
  [`FERMUT_CACHE_KEY`](../reference/environment-variables.md) as a masked
  CI/CD variable.
- **Onboarding:** start with `allow_failure: true` on the job, then remove
  it once the team trusts the gate. See [Rolling out to a team](projects.md).

### Nightly sweep, sharded

GitLab's `parallel:` sets `CI_NODE_INDEX` (1-based) and `CI_NODE_TOTAL`,
which map straight onto `--shard i/n`. A final job merges the shards and
records one history entry.

```yaml
fermut-shard:
  image: python:3.12
  rules:
    - if: $CI_PIPELINE_SOURCE == "schedule"
  parallel: 4
  script:
    - pip install fermut pytest coverage pytest-cov -e .
    - fermut coverage
    - fermut run src/ --tests tests/ --shard "$CI_NODE_INDEX/$CI_NODE_TOTAL"
        --no-fail --json "shard-$CI_NODE_INDEX.json"
  artifacts:
    paths: ["shard-*.json"]

fermut-merge:
  image: python:3.12
  rules:
    - if: $CI_PIPELINE_SOURCE == "schedule"
  needs: [fermut-shard]
  cache:
    key: fermut-history
    paths: [.fermut/history.jsonl]
  script:
    - pip install fermut
    - fermut merge shard-*.json --expect-shards 4
        --json merged.json --markdown merged.md --junit merged.xml
        --max-errored 0 --history entry.jsonl
    - mkdir -p .fermut && cat entry.jsonl >> .fermut/history.jsonl
    - fermut trend --history-path .fermut/history.jsonl --fail-on-regression 1
  artifacts:
    when: always
    paths: [merged.json, merged.md]
    reports:
      junit: merged.xml
```

[`fermut merge`](../reference/cli/merge.md) refuses a missing shard and
shards run with different settings, so a lost job fails the merge instead
of quietly lowering the score. `--history` writes the merged run's
single history entry to its own file (it overwrites, so don't point it at
the log); the next line appends it. `--max-errored 0` keeps a run with
errored mutants out of the trend, and `trend --fail-on-regression 1` fails
the pipeline when the score drops more than a point. Each shard builds its own `.coverage` here;
to build it once and share it, see [Coverage in CI](ci-coverage.md).

## Jenkins

A declarative pipeline for a multibranch job. `CHANGE_TARGET` is set on
pull/merge-request builds, so the same `Jenkinsfile` gates PRs and runs a
full sweep on branch builds.

```groovy
pipeline {
  agent { docker { image 'python:3.12' } }
  environment {
    FERMUT_CACHE_KEY = credentials('fermut-cache-key')   // optional: signs the cache
  }
  stages {
    stage('Install') {
      steps {
        sh 'pip install fermut pytest coverage pytest-cov -e .'
      }
    }
    stage('Coverage') {
      steps { sh 'fermut coverage' }
    }
    stage('Mutation') {
      steps {
        script {
          def scope = env.CHANGE_TARGET
            ? "--diff-only origin/${env.CHANGE_TARGET}"
            : ''
          if (env.CHANGE_TARGET) {
            sh "git fetch origin ${env.CHANGE_TARGET}:refs/remotes/origin/${env.CHANGE_TARGET}"
          }
          // non-zero here = the run broke, not the gate
          sh """
            fermut run src/ --tests tests/ ${scope} \\
              --no-verify-baseline --no-fail \\
              --json report.json --markdown report.md --junit report.xml
          """
        }
      }
    }
    stage('Gate') {
      steps { sh 'python ci/fermut_gate.py report.json' }
    }
  }
  post {
    always {
      junit allowEmptyResults: true, testResults: 'report.xml'
      archiveArtifacts artifacts: 'report.*', allowEmptyArchive: true
    }
  }
}
```

- **Full history:** `--diff-only` needs the merge base. If your checkout
  is shallow (the Git plugin's *Shallow clone* option), turn that off or
  fetch enough depth.
- **Reports:** `--junit` feeds the JUnit plugin, so survivors show as
  failed test cases; `report.md` is archived for people. Posting it to the
  PR is up to your SCM plugin.
- **Caching:** Jenkins agents are often ephemeral. To reuse verdicts across
  builds, persist `.fermut/` the way you persist other build caches (a
  shared workspace, or stash/copy it to object storage between builds).
  An uncached run is still correct, just slower.

## Where next

- **[CI recipes](integrations.md)**: GitHub Actions versions of the same
  patterns, plus history persistence.
- **[Trends](trends.md)**: what the history log feeds.
- **[Enterprise and offline installs](../getting-started/enterprise-install.md)**:
  internal mirrors, verification, containers.
