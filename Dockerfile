FROM python:3.12-slim

# libgomp1: OpenMP runtime for onnxruntime; libglib2.0-0: opencv-headless
RUN apt-get update \
 && apt-get install -y --no-install-recommends libgomp1 libglib2.0-0 \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY pyproject.toml README.md ./
COPY src ./src
RUN pip install --no-cache-dir .

# Bake the model into the image so the container needs no internet at runtime.
COPY scripts ./scripts
RUN python scripts/fetch_model.py --out /app/models/model-weights.onnx

ENV PRUSA_WATCH_CONFIG=/config/config.yaml \
    PYTHONUNBUFFERED=1
VOLUME ["/app/data"]
EXPOSE 8484

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s \
  CMD python -c "import urllib.request,sys; urllib.request.urlopen('http://127.0.0.1:8484/api/state', timeout=4)" || exit 1

ENTRYPOINT ["prusa-watch"]
CMD ["run"]
