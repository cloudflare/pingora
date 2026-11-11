target "pingora-debian" {
  context = "."
  dockerfile = "Dockerfile"
  tags = ["pingora:latest"]
  platforms = ["linux/amd64", "linux/arm64"]
}

target "pingora-alpine" {
  context = "."
  dockerfile = "Dockerfile-alpine"
  tags = ["pingora:latest"]
  platforms = ["linux/amd64", "linux/arm64"]
}
