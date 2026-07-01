FROM python:3.12-slim
WORKDIR /worker
COPY pyproject.toml pyproject.toml
RUN pip install --no-cache-dir .
COPY . .
CMD ["python", "worker.py"]
