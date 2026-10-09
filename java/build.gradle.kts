// java/build.gradle.kts

// The Java side of the Gabion node protocol: the library a Gabion application
// uses to talk to the controller.
//
// Its message types are not written here. The build generates them from the
// protocol description — the same file the controller's Rust types come
// from — and compiles them together with the hand-written part: the checked
// decoding and the audio frames. `check` then runs the whole library through
// the shared vectors, the same files the Rust tests read.

import net.ltgt.gradle.errorprone.errorprone

plugins {
    `java-library`
    // Null is checked when the library compiles, as in the framework it is
    // compiled into: NullAway, run by Error Prone, of the same versions.
    id("net.ltgt.errorprone") version "4.1.0"
}

group = "dev.gabion"
version = "0.1.0"

val protocolPackage = "dev.gabion.telephony.protocol"
val repositoryRoot = layout.projectDirectory.dir("..")
val protocolDescription = repositoryRoot.file("protocol/node-protocol.schema.json")
val generatedSources = layout.buildDirectory.dir("generated/sources/protocol")

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
}

val generateProtocol by tasks.registering(Exec::class) {
    description = "Generates the Java message types from the protocol description."
    group = "build"

    inputs.file(protocolDescription)
    inputs.dir(repositoryRoot.dir("crates/protocol-java/src"))
    outputs.dir(generatedSources)

    workingDir = repositoryRoot.asFile
    commandLine(
        "cargo", "run", "--quiet", "--locked", "-p", "protocol-java", "--",
        protocolDescription.asFile.path,
        protocolPackage,
        generatedSources.get().asFile.path,
    )
    // Generated files of a definition that no longer exists must not survive.
    doFirst { delete(generatedSources) }
}

sourceSets {
    main {
        java.srcDir(files(generatedSources).builtBy(generateProtocol))
        // The generator puts the description next to the types: the checked
        // decoding reads it as a resource of its own package.
        resources.srcDir(files(generatedSources).builtBy(generateProtocol))
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

tasks.processResources {
    exclude("**/*.java")
}

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
