FROM gcr.io/oss-fuzz-base/base-builder-rust
RUN apt-get update && apt-get install -y zip
COPY . $SRC/fermut
WORKDIR $SRC/fermut
COPY .clusterfuzzlite/build.sh $SRC/
