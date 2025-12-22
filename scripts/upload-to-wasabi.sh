#!/bin/bash
set -e

# Voice Bird Desktop - Wasabi Upload Script
# Uploads macOS build artifacts to Wasabi S3-compatible storage

echo "=== Voice Bird Desktop - Wasabi Upload ==="

# Load environment variables
load_env() {
    SCRIPT_DIR="$(dirname "$0")"
    ENV_FILE="$SCRIPT_DIR/../.env"

    if [ -f "$ENV_FILE" ]; then
        echo "Loading credentials from .env..."
        export $(grep -E '^WASABI_' "$ENV_FILE" | xargs)
    fi

    # Validate required variables
    if [ -z "$WASABI_ACCESS_KEY_ID" ] || [ -z "$WASABI_SECRET_ACCESS_KEY" ]; then
        echo "ERROR: Wasabi credentials not found. Set these in .env:"
        echo "  WASABI_ACCESS_KEY_ID"
        echo "  WASABI_SECRET_ACCESS_KEY"
        echo "  WASABI_REGION"
        echo "  WASABI_BUCKET_NAME"
        echo "  WASABI_ENDPOINT"
        exit 1
    fi

    # Set defaults
    WASABI_REGION="${WASABI_REGION:-eu-central-2}"
    WASABI_BUCKET_NAME="${WASABI_BUCKET_NAME:-voice-bird-europe}"
    WASABI_ENDPOINT="${WASABI_ENDPOINT:-https://s3.eu-central-2.wasabisys.com}"
}

# Check for AWS CLI
check_aws_cli() {
    if ! command -v aws &> /dev/null; then
        echo "AWS CLI not found. Installing..."
        if command -v brew &> /dev/null; then
            brew install awscli
        else
            echo "ERROR: Please install AWS CLI manually:"
            echo "  brew install awscli"
            echo "  or: pip install awscli"
            exit 1
        fi
    fi
}

# Configure AWS CLI for Wasabi
configure_aws() {
    export AWS_ACCESS_KEY_ID="$WASABI_ACCESS_KEY_ID"
    export AWS_SECRET_ACCESS_KEY="$WASABI_SECRET_ACCESS_KEY"
    export AWS_DEFAULT_REGION="$WASABI_REGION"
}

# Upload artifacts
upload_artifacts() {
    SCRIPT_DIR="$(dirname "$0")"
    PROJECT_DIR="$SCRIPT_DIR/.."
    BUNDLE_DIR="$PROJECT_DIR/target/release/bundle"

    VERSION=$(grep '^version' "$PROJECT_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')
    TIMESTAMP=$(date +%Y%m%d-%H%M%S)
    UPLOAD_PREFIX="releases/macos/v${VERSION}"

    echo ""
    echo "Version: $VERSION"
    echo "Upload path: s3://$WASABI_BUCKET_NAME/$UPLOAD_PREFIX/"
    echo ""

    # Upload DMG if exists
    if [ -d "$BUNDLE_DIR/dmg" ]; then
        for dmg in "$BUNDLE_DIR/dmg/"*.dmg; do
            if [ -f "$dmg" ]; then
                FILENAME=$(basename "$dmg")
                echo "Uploading $FILENAME..."
                aws s3 cp "$dmg" "s3://$WASABI_BUCKET_NAME/$UPLOAD_PREFIX/$FILENAME" \
                    --endpoint-url "$WASABI_ENDPOINT" \
                    --content-type "application/x-apple-diskimage"
                echo "  -> s3://$WASABI_BUCKET_NAME/$UPLOAD_PREFIX/$FILENAME"
            fi
        done
    fi

    # Upload .app bundle as zip if exists
    if [ -d "$BUNDLE_DIR/macos" ]; then
        for app in "$BUNDLE_DIR/macos/"*.app; do
            if [ -d "$app" ]; then
                APP_NAME=$(basename "$app" .app)
                ZIP_NAME="${APP_NAME}-${VERSION}-macos.zip"
                ZIP_PATH="/tmp/$ZIP_NAME"

                echo "Creating zip archive of $APP_NAME.app..."
                cd "$BUNDLE_DIR/macos"
                zip -r "$ZIP_PATH" "$(basename "$app")"
                cd - > /dev/null

                echo "Uploading $ZIP_NAME..."
                aws s3 cp "$ZIP_PATH" "s3://$WASABI_BUCKET_NAME/$UPLOAD_PREFIX/$ZIP_NAME" \
                    --endpoint-url "$WASABI_ENDPOINT" \
                    --content-type "application/zip"
                echo "  -> s3://$WASABI_BUCKET_NAME/$UPLOAD_PREFIX/$ZIP_NAME"

                rm "$ZIP_PATH"
            fi
        done
    fi

    # Also upload to latest/
    echo ""
    echo "Copying to latest/..."
    aws s3 sync "s3://$WASABI_BUCKET_NAME/$UPLOAD_PREFIX/" \
        "s3://$WASABI_BUCKET_NAME/releases/macos/latest/" \
        --endpoint-url "$WASABI_ENDPOINT"
}

# Show upload results
show_results() {
    echo ""
    echo "=== Upload Complete ==="
    echo ""
    echo "Files available at:"
    aws s3 ls "s3://$WASABI_BUCKET_NAME/releases/macos/" \
        --endpoint-url "$WASABI_ENDPOINT" \
        --recursive

    echo ""
    echo "Download URLs (public if bucket allows):"
    VERSION=$(grep '^version' "$(dirname "$0")/../Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')
    echo "  $WASABI_ENDPOINT/$WASABI_BUCKET_NAME/releases/macos/v${VERSION}/"
    echo "  $WASABI_ENDPOINT/$WASABI_BUCKET_NAME/releases/macos/latest/"
}

# Main execution
main() {
    load_env
    check_aws_cli
    configure_aws
    upload_artifacts
    show_results
}

main "$@"
