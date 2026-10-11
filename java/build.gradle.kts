// java/build.gradle.kts

// The Java side of the Gabion node protocol: the library a Gabion application
// uses to talk to the controller.
//
// Its message types are not written here. The build generates them from the
// protocol description — the same file the controller's Rust types come
// from — and compiles them together with the hand-written part: the checked
// decoding and the audio frames. `check` then runs the whole library through
// the shared vectors, the same files the Rust tests read.
//
// The library is published to Maven Central with every release of the
// controller, under the controller's version: an application takes it as an
// ordinary dependency, and the build of that application needs nothing of
// Gabion's. `centralBundle` lays out what Central takes — the library, its
// sources and documentation, the POM, a signature and checksums for each —
// and the release uploads it.

import net.ltgt.gradle.errorprone.errorprone

plugins {
    `java-library`
    `maven-publish`
    signing
    // Null is checked when the library compiles, as in the framework it is
    // compiled into: NullAway, run by Error Prone, of the same versions.
    id("net.ltgt.errorprone") version "4.1.0"
}

group = "dev.gabion"

// One version for the controller and the library that speaks to it: the
// controller names its version to the application in every hello, and the
// library of the same version is the one that speaks its protocol. The
// controller's own manifest is its one owner.
val controllerManifest = layout.projectDirectory.file("../crates/asterisk-controller/Cargo.toml")
version = providers.fileContents(controllerManifest).asText.map { manifest ->
    Regex("""(?m)^version = "([^"]+)"$""").find(manifest)?.groupValues?.get(1)
        ?: throw GradleException("No version in ${controllerManifest.asFile}")
}.get()

val protocolPackage = "dev.gabion.telephony.protocol"
val repositoryRoot = layout.projectDirectory.dir("..")
val protocolDescription = repositoryRoot.file("protocol/node-protocol.schema.json")
val generatedSources = layout.buildDirectory.dir("generated/sources/protocol")
val generatedResources = layout.buildDirectory.dir("generated/resources/protocol")

// Everything this build is compiled against or runs is checked against the
// checksums pinned in gradle/verification-metadata.xml. The one thing that
// file trusts without a checksum is the source and documentation archives of
// those libraries: an editor asks for them to show a library's code, and
// nothing in them is ever compiled or run.
repositories {
    mavenCentral()
}

dependencies {
    // The versions the Gabion framework itself uses: the library is compiled
    // into an application built on them.
    api("tools.jackson.core:jackson-databind:3.0.3")
    api("org.jspecify:jspecify:1.0.0")

    errorprone("com.google.errorprone:error_prone_core:2.36.0")
    errorprone("com.uber.nullaway:nullaway:0.12.3")
}

java {
    toolchain {
        languageVersion = JavaLanguageVersion.of(21)
    }
    // Central takes a library only with its sources and its documentation.
    withSourcesJar()
    withJavadocJar()
}

val generateProtocol by tasks.registering(Exec::class) {
    description = "Generates the Java message types from the protocol description."
    group = "build"

    inputs.file(protocolDescription)
    inputs.dir(repositoryRoot.dir("crates/protocol-java/src"))
    outputs.dir(generatedSources)
    outputs.dir(generatedResources)

    workingDir = repositoryRoot.asFile
    commandLine(
        "cargo", "run", "--quiet", "--locked", "-p", "protocol-java", "--",
        protocolDescription.asFile.path,
        protocolPackage,
        generatedSources.get().asFile.path,
    )
    // Generated files of a definition that no longer exists must not survive.
    doFirst { delete(generatedSources, generatedResources) }
    // The generator puts the description next to the types, in their package.
    // It is a resource, not a source: it moves to a directory of its own, so
    // that every reader of the source set — the sources jar among them —
    // finds each file in exactly one place.
    doLast {
        val sources = generatedSources.get().asFile
        val resources = generatedResources.get().asFile
        sources.walkTopDown().filter { it.isFile && it.extension != "java" }.forEach { file ->
            val target = resources.resolve(file.relativeTo(sources))
            target.parentFile.mkdirs()
            if (!file.renameTo(target)) {
                throw GradleException("Could not move $file to $target")
            }
        }
    }
}

sourceSets {
    main {
        java.srcDir(files(generatedSources).builtBy(generateProtocol))
        // The checked decoding reads the description as a resource of its own
        // package.
        resources.srcDir(files(generatedResources).builtBy(generateProtocol))
    }
}

// The programs that hold the library to the controller have a source set of
// their own: one runs it through the shared vectors, the other through a real
// exchange with the controller. They are not tests in the build tool's sense
// — each is one program with one verdict — and are run by the `vectorCheck`
// and `serviceCheck` tasks below.
val vectors: SourceSet by sourceSets.creating {
    compileClasspath += sourceSets.main.get().output
    runtimeClasspath += sourceSets.main.get().output
}

configurations[vectors.implementationConfigurationName].extendsFrom(configurations.api.get())

tasks.withType<JavaCompile>().configureEach {
    options.release = 21
    // Every warning the compiler knows is an error — Error Prone's too:
    // generated code is held to the same standard as code written by hand.
    options.compilerArgs.addAll(listOf("-Xlint:all", "-Werror", "-proc:none"))
    options.errorprone {
        // Every package of the library is @NullMarked: what may be null says
        // so, and NullAway holds every use to it.
        option("NullAway:AnnotatedPackages", "dev.gabion")
        error("NullAway")
    }
}

val vectorCheck by tasks.registering(JavaExec::class) {
    description = "Runs the library through the vectors it shares with the controller."
    group = "verification"

    val messages = repositoryRoot.file("protocol/messages.vectors.json")
    val audioFrames = repositoryRoot.file("protocol/audio-frames.vectors.json")
    val fingerprints = repositoryRoot.file("protocol/settings-fingerprint.vectors.json")
    inputs.files(messages, audioFrames, fingerprints)

    classpath = vectors.runtimeClasspath
    mainClass = "dev.gabion.telephony.protocol.checks.VectorCheck"
    args(protocolPackage, messages.asFile.path, audioFrames.asFile.path, fingerprints.asFile.path)
}

val buildController by tasks.registering(Exec::class) {
    description = "Builds the controller the service check talks to."
    group = "build"

    workingDir = repositoryRoot.asFile
    commandLine("cargo", "build", "--quiet", "--locked", "-p", "asterisk-controller")
}

val serviceCheck by tasks.registering(JavaExec::class) {
    description = "Plays the application on the controller's service connection with this library."
    group = "verification"

    dependsOn(buildController)
    classpath = vectors.runtimeClasspath
    mainClass = "dev.gabion.telephony.protocol.checks.ServiceCheck"
    args(repositoryRoot.file("target/debug/asterisk-controller").asFile.path)
}

tasks.check {
    dependsOn(vectorCheck, serviceCheck)
}

// The documentation published with the library. Every warning of the
// documentation tool is an error, as every warning of the compiler is — save
// one group: a missing comment. The generated types say what the protocol
// description says about them, and where the description is silent the
// generator has no words to give; it does not invent them.
tasks.javadoc {
    (options as StandardJavadocDocletOptions).apply {
        addBooleanOption("Xdoclint:all,-missing", true)
        addBooleanOption("Werror", true)
    }
}

// What is written by hand is documented in full: a missing comment there is
// an error. The generated types are on its classpath, not in its sources.
val handwrittenJavadoc by tasks.registering(Javadoc::class) {
    description = "Checks that the hand-written part of the library is documented in full."
    group = "verification"

    source = fileTree("src/main/java")
    classpath = sourceSets.main.get().compileClasspath + sourceSets.main.get().output
    setDestinationDir(layout.buildDirectory.dir("tmp/handwritten-javadoc").get().asFile)
    (options as StandardJavadocDocletOptions).apply {
        addBooleanOption("Xdoclint:all", true)
        addBooleanOption("Werror", true)
    }
}

tasks.check {
    dependsOn(handwrittenJavadoc)
}

publishing {
    publications {
        create<MavenPublication>("library") {
            from(components["java"])
            // The protocol has one name on both sides: the Rust crate and the
            // description are `node-protocol` too.
            artifactId = "node-protocol"
            pom {
                name = "Gabion node protocol"
                description = "The Java side of the protocol a Gabion application speaks with the controller " +
                    "of its telephony node: the message types, generated from the protocol description, the " +
                    "checked decoding and the audio frames."
                url = "https://github.com/gabion-dev/asterisk-controller"
                licenses {
                    license {
                        name = "Business Source License 1.1"
                        url = "https://github.com/gabion-dev/asterisk-controller/blob/main/LICENSE"
                    }
                }
                developers {
                    developer {
                        name = "Pikiner, Inc."
                        organization = "Pikiner, Inc."
                        organizationUrl = "https://github.com/gabion-dev"
                    }
                }
                scm {
                    connection = "scm:git:https://github.com/gabion-dev/asterisk-controller.git"
                    developerConnection = "scm:git:https://github.com/gabion-dev/asterisk-controller.git"
                    url = "https://github.com/gabion-dev/asterisk-controller"
                }
            }
        }
    }
    repositories {
        // The layout Central takes, in a directory of its own: `centralBundle`
        // packs it, the release uploads the archive.
        maven {
            name = "bundle"
            url = uri(layout.buildDirectory.dir("central-bundle"))
        }
    }
}

// Central refuses a file without its signature, so a bundle is never laid
// out unsigned: without the key the build stops here, not at Central. The
// key comes from the environment of the release, armored, with its
// passphrase; it is never in the repository.
signing {
    val key = providers.environmentVariable("SIGNING_KEY")
    val passphrase = providers.environmentVariable("SIGNING_PASSWORD")
    useInMemoryPgpKeys(key.orNull, passphrase.orNull)
    sign(publishing.publications["library"])
}

val bundleDirectory = layout.buildDirectory.dir("central-bundle")

tasks.withType<Sign>().configureEach {
    doFirst {
        if (System.getenv("SIGNING_KEY").isNullOrEmpty()) {
            throw GradleException(
                "SIGNING_KEY is not set: Central refuses unsigned files. Set SIGNING_KEY to the armored " +
                    "private key and SIGNING_PASSWORD to its passphrase.",
            )
        }
    }
}

tasks.named("publishLibraryPublicationToBundleRepository") {
    // A bundle holds this version and nothing left from an earlier one.
    doFirst { delete(bundleDirectory) }
}

val centralBundle by tasks.registering(Zip::class) {
    description = "Packs the library, its sources, documentation, POM, signatures and checksums for Central."
    group = "publishing"

    dependsOn("publishLibraryPublicationToBundleRepository")
    from(bundleDirectory)
    // The repository's own index is not a part of a component, Central builds
    // its own; a signature is not given checksums of its own — what Central
    // asks for is a signature and checksums for each file of the component.
    exclude("**/maven-metadata.xml*")
    exclude("**/*.asc.*")
    archiveFileName = "central-bundle.zip"
    destinationDirectory = layout.buildDirectory.dir("distributions")
}
