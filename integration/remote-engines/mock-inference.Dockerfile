FROM python:3.13-alpine

WORKDIR /app
COPY mock_inference.py .

EXPOSE 8080
CMD ["python", "mock_inference.py"]
