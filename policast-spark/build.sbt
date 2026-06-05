name := "policast-spark"
version := "0.1.0"
scalaVersion := "2.13.16"

val sparkVersion = "4.1.2"

// Optional Maven mirror: when MAVEN_PROXY_URL is set in the environment
// (e.g. a corporate firewall proxy), point sbt at it as a single resolver.
// Otherwise fall back to a public OSS resolver list. Maven Central is
// included transitively as sbt's built-in default in both cases.
resolvers ++= {
  sys.env.get("MAVEN_PROXY_URL").map(_.trim).filter(_.nonEmpty) match {
    case Some(url) =>
      Seq("Maven Proxy" at url)
    case None =>
      Seq(
        "Artima Maven Repository"      at "https://repo.artima.com/releases",
        "scala-tools"                  at "https://oss.sonatype.org/content/groups/scala-tools",
        "sonatype-releases"            at "https://oss.sonatype.org/content/repositories/releases/",
        "Typesafe repository"          at "https://dl.bintray.com/typesafe/ivy-releases/",
        "Second Typesafe repo"         at "https://dl.bintray.com/typesafe/maven-releases/",
        "Mesosphere Public Repository" at "https://downloads.mesosphere.io/maven",
        "Maven Repo"                   at "https://repo1.maven.org/maven2/",
        "Maven Repo2"                  at "https://repo2.maven.org/maven2/",
        Resolver.sonatypeRepo("public")
      )
  }
}

libraryDependencies ++= Seq(
  "org.apache.spark" %% "spark-sql"      % sparkVersion % "provided",
  "org.apache.spark" %% "spark-catalyst" % sparkVersion % "provided",
  "org.apache.spark" %% "spark-core"     % sparkVersion % "provided",

  "dev.cel"              %  "cel"       % "0.12.0",
  "com.google.code.gson" %  "gson"     % "2.11.0",

  "org.scalatest"    %% "scalatest"     % "3.2.19"     % "test"
)

// The enforcement spec runs Spark in-process. On Java 17 a plain forked JVM
// lacks the module-opens that spark-submit/spark-shell add by default, so
// Spark's Platform/Unsafe access throws InaccessibleObjectException. Fork the
// test JVM with the same opens, and keep the single SparkSession serialized.
Test / fork := true
Test / parallelExecution := false
Test / javaOptions ++= Seq(
  "-Xmx2g",
  "-XX:+IgnoreUnrecognizedVMOptions",
  "--add-opens=java.base/java.lang=ALL-UNNAMED",
  "--add-opens=java.base/java.lang.invoke=ALL-UNNAMED",
  "--add-opens=java.base/java.lang.reflect=ALL-UNNAMED",
  "--add-opens=java.base/java.io=ALL-UNNAMED",
  "--add-opens=java.base/java.net=ALL-UNNAMED",
  "--add-opens=java.base/java.nio=ALL-UNNAMED",
  "--add-opens=java.base/java.util=ALL-UNNAMED",
  "--add-opens=java.base/java.util.concurrent=ALL-UNNAMED",
  "--add-opens=java.base/java.util.concurrent.atomic=ALL-UNNAMED",
  "--add-opens=java.base/jdk.internal.ref=ALL-UNNAMED",
  "--add-opens=java.base/sun.nio.ch=ALL-UNNAMED",
  "--add-opens=java.base/sun.nio.cs=ALL-UNNAMED",
  "--add-opens=java.base/sun.security.action=ALL-UNNAMED",
  "--add-opens=java.base/sun.util.calendar=ALL-UNNAMED",
  "--add-opens=java.security.jgss/sun.security.krb5=ALL-UNNAMED"
)

assembly / assemblyMergeStrategy := {
  case PathList("META-INF", _*) => MergeStrategy.discard
  case _                        => MergeStrategy.first
}
