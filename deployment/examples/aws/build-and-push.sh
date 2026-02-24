#!/bin/bash
# Build and push Docker image to AWS ECR

set -e

# Configuration
AWS_REGION="${AWS_REGION:-us-east-1}"
AWS_ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
ECR_REPOSITORY="${ECR_REPOSITORY:-mo-agent}"
IMAGE_TAG="${IMAGE_TAG:-latest}"

ECR_URL="${AWS_ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com"
FULL_IMAGE="${ECR_URL}/${ECR_REPOSITORY}:${IMAGE_TAG}"

echo "🔨 Building Docker image..."
docker build -t ${ECR_REPOSITORY}:${IMAGE_TAG} ../../..

echo "🏷️  Tagging image..."
docker tag ${ECR_REPOSITORY}:${IMAGE_TAG} ${FULL_IMAGE}

echo "🔐 Logging in to ECR..."
aws ecr get-login-password --region ${AWS_REGION} | \
  docker login --username AWS --password-stdin ${ECR_URL}

echo "📤 Pushing image to ECR..."
docker push ${FULL_IMAGE}

echo "✅ Image pushed successfully!"
echo "   Image: ${FULL_IMAGE}"
