import os

from hatchling.builders.hooks.plugin.interface import BuildHookInterface
from hatchling.metadata.plugin.interface import MetadataHookInterface


class CustomMetadataHook(MetadataHookInterface):
    def update(self, metadata):
        readme_path = os.path.join(self.root, "README.md")
        if not os.path.isfile(readme_path):
            readme_path = os.path.normpath(os.path.join(self.root, "..", "..", "README.md"))

        with open(readme_path, encoding="utf-8") as readme_file:
            metadata["readme"] = {"content-type": "text/markdown", "text": readme_file.read()}


class CustomBuildHook(BuildHookInterface):
    def initialize(self, version, build_data):
        build_data["pure_python"] = False
        build_data["infer_tag"] = True
