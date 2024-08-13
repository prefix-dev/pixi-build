from __future__ import annotations

import json
from typing import Annotated, List, Union
from pydantic import (
    BaseModel,
    Field,
    TypeAdapter,
    DirectoryPath,
    UrlConstraints,
    constr,
    NonNegativeInt,
    AnyHttpUrl
)
from pydantic_core import Url

NonEmptyStr = constr(min_length=1)

Platform = NonEmptyStr

CondaUrl = Annotated[Url, UrlConstraints(allowed_schemes=['http', 'https', 'file'])]

# TODO: Add regex maybe?
PackageName = NonEmptyStr
Version = NonEmptyStr


# =============================================
# RPC: initialize
# =============================================
class BackendCapabilities(BaseModel):
    providesCondaMetadata: bool | None = Field(
        None,
        description="Whether the backend is capable of providing metadata about source packages",
    )


class FrontendCapabilities(BaseModel):
    pass


class InitializeParams(BaseModel):
    """
    The params send as part  of the `initialize` rpc method. The expected result is of type `InitializeResult`.
    """

    sourceDir: DirectoryPath = Field(
        ...,
        description="An absolute path to the directory that contains the source files",
    )

    capabilities: FrontendCapabilities = Field(
        ..., description="Capabilities provided by the frontend"
    )


class InitializeResult(BaseModel):
    """
    The result of the `initialize` rpc method.
    """

    capabilities: BackendCapabilities


# =============================================
# RPC: condaMetadata
#
# This is used to determine metadata of the conda packages in the source directory.
# =============================================
class CondaMetadataParams(BaseModel):
    targetPlatform: Platform | None = Field(
        None,
        description="The target platform, or the current platform if not specified",
    )
    channelBaseUrls: List[CondaUrl] = Field(None, description="Urls of channels to use for any resolution.")


class CondaPackageMetadata(BaseModel):
    name: PackageName = Field(..., description="The name of the package")
    version: Version = Field(..., description="The version of the package")
    build: NonEmptyStr = Field(..., description="The build string of the package")
    buildNumber: NonNegativeInt = Field(
        0, description="The build number of the package"
    )
    subdir: Platform = Field(..., description="The subdirectory of the package")
    depends: List[NonEmptyStr] | None = Field(
        None, description="The dependencies of the package"
    )
    constrains: List[NonEmptyStr] | None = Field(
        None, description="Additional run constraints that apply to the package"
    )
    license: NonEmptyStr | None = Field(None, description="The license of the package")
    licenseFamily: NonEmptyStr | None = Field(
        None, description="The license family of the package"
    )


class CondaMetadataResult(BaseModel):
    packages: List[CondaPackageMetadata]


Schema = TypeAdapter(
    Union[InitializeParams, InitializeResult, CondaMetadataParams, CondaMetadataResult]
)


if __name__ == "__main__":
    print(json.dumps(Schema.json_schema(), indent=2))
