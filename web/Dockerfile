# rargo.rs site: Flask serving the static site/ directory, with room for the
# phase-2 accounts/dashboard routes. SQLite will live on the /data volume
# (mounted by deploy.sh) so state survives redeploys.
# Dependencies are managed with uv (pyproject.toml + uv.lock).
FROM python:3.12-alpine
COPY --from=ghcr.io/astral-sh/uv:latest /uv /usr/local/bin/uv

WORKDIR /app

# Install locked dependencies first so this layer caches across app edits.
COPY pyproject.toml uv.lock ./
RUN uv sync --frozen --no-dev

COPY app.py .
COPY site/ site/

ENV PATH="/app/.venv/bin:$PATH"
EXPOSE 80
CMD ["waitress-serve", "--listen=0.0.0.0:80", "--threads=8", "app:app"]
