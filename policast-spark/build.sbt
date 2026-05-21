name := "policast-spark"
version := "0.1.0"
scalaVersion := "2.13.14"

val sparkVersion = "3.5.4"

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

assembly / assemblyMergeStrategy := {
  case PathList("META-INF", _*) => MergeStrategy.discard
  case _                        => MergeStrategy.first
}
