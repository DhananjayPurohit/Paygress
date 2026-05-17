# paygress/ngit-runner image — one-shot CI/CD pipeline executor.
#
# Layer choices:
# - python:3.12-slim base. Python is the entrypoint language (yaml
#   parsing + subprocess orchestration are cleanest there). Slim
#   keeps the pull fast.
# - Layered tools: git (clone), curl + jq (small fetches in pipeline
#   steps), build-essential (compile-from-source common to many CI
#   matrices), nodejs/npm + go (popular runtime stacks). Rust is
#   heavier — repos that need it should run `rustup` from their
#   pipeline step instead of paying for a Rust toolchain in every
#   ngit-runner pull.
# - PyYAML for parsing .ngit/ci.yml. No other Python deps — the
#   entrypoint is stdlib-only beyond yaml.
#
# Build (locally):
#   docker build -t ghcr.io/dhananjaypurohit/paygress-ngit-runner:0.1.0 \
#                images/ngit-runner
#
# CI publishes on tags `ngit-runner-v*` via
# .github/workflows/ngit-runner-image.yml (follow-up).

FROM python:3.12-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        git ca-certificates curl jq build-essential \
        nodejs npm golang \
    && rm -rf /var/lib/apt/lists/* \
    && pip install --no-cache-dir pyyaml

# Workspace is ephemeral — one-shot CI run, no persistence.
RUN mkdir -p /workspace && chmod 0777 /workspace
WORKDIR /workspace

COPY entrypoint.py /usr/local/bin/ngit-runner
RUN chmod +x /usr/local/bin/ngit-runner

ENV NGIT_PIPELINE_PATH=.ngit/ci.yml \
    NGIT_STATUS_PORT=8080 \
    PYTHONUNBUFFERED=1

EXPOSE 8080

ENTRYPOINT ["python3", "/usr/local/bin/ngit-runner"]
