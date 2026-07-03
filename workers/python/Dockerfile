FROM python:3.12-slim
WORKDIR /worker
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg \
    && rm -rf /var/lib/apt/lists/*
COPY pyproject.toml pyproject.toml
RUN pip install --no-cache-dir .
COPY . .
CMD ["python", "worker.py"]
